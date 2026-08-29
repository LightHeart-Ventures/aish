# ADR: Webhook plugin manifest schema & event routing (SPR-069)

- **Status:** Proposed
- **Sprint:** SPR-069
- **Related cards:** TASK-376 (schema reconciliation), TASK-378 (broker ingress
  routing), TASK-379 (handler dispatch), TASK-386/387 (GitHub reference plugin),
  TASK-446 (shell-injection guard), TASK-447 (schema-fork removal), TASK-448
  (routing ADR & source-routing backlog).

## Context

SPR-069 reconciles an in-flight webhook design against what already shipped in
the `aish-webhook-client` crate. A peer review found that most of the "net-new"
work was already implemented on the crate side, so the sprint was rescoped to
**adopt the shipped implementation** and **document the canonical contract**
rather than rebuild it. This ADR records the two decisions that unblock the
remaining wiring cards: the canonical manifest schema, and the event routing
model.

## Decision 1 — Canonical manifest schema

The single source of truth for webhook handlers is the `webhooks[]` array in a
plugin's `plugin.json`, as parsed by
`aish-webhook-client::dispatcher::PluginManifest`:

```jsonc
{
  "id": "github",
  "name": "GitHub Webhook Handlers",
  "version": "0.1.0",
  "webhooks": [
    {
      "event_type": "pull_request",     // exact match, or "*" for all events
      "command": ["handlers/pr-review.sh", "..."], // argv; command[0] = program
      "filters": { "action": "opened" },// AND-combined dotted-path equality
      "timeout_secs": 30                // optional; default 30s
    }
  ]
}
```

Reconciliation rules:

- **`webhooks` is canonical**; `handlers` was accepted as a serde `alias` for
  one release so older manifests kept loading, but TASK-447 removed the fork —
  a manifest still using the legacy top-level `handlers` key now fails fast
  (skipped with an actionable warning) instead of silently loading zero
  handlers. Every first-party manifest uses `webhooks`.
- The aish core loader (`src/plugins.rs::PluginManifest`) and the webhook-client
  loader are **both lenient** (`#[serde(default)]` on optional fields, unknown
  keys dropped), so one manifest satisfies both. There is **no** competing
  `webhook_handlers` field — the earlier "schema fork" does not exist on `main`.
- `command` is **argv**, never a shell string. This is load-bearing for security
  (Decision 3).

## Decision 2 — Event routing model

Ingress → dispatch flows as:

1. **Ingress** verifies the source signature (e.g. GitHub `X-Hub-Signature-256`
   HMAC, constant-time compare — TASK-377) and normalizes the delivery into a
   `Webhook { id, tenant_id, plugin_id, event_type, payload }` envelope.
2. **Routing key** is `(tenant_id, event_type)`. `PluginRegistry::matching()`
   returns every `(plugin_id, handler)` whose `event_type` equals the event or is
   `"*"`.
3. **Filtering** applies each handler's AND-combined `filters` over dotted
   payload paths; non-matching handlers are recorded as `skipped` (still
   audited), not dropped silently.
4. **Dispatch** fork/exec's every surviving handler **concurrently** with full
   failure isolation: one handler failing, panicking, or timing out never blocks
   another. Each handler is given the payload on stdin and `WEBHOOK_*` env vars,
   and is bounded by `timeout_secs` (kill-on-drop).
5. **Audit** — every outcome (executed / filtered / failed) is written to the
   `AuditSink` (TASK-380 extends this trait with a bounded plugin-memory ring
   sink). A sink write failure is logged and swallowed; it never breaks dispatch.

Source-based routing (routing distinct *sources* — Slack, Mailgun, Fly, GitHub —
to different signature validators and namespaces) is **out of scope for SPR-069**
and captured as a backlog item (TASK-448 follow-up): add a `source` discriminator
to the envelope and a per-source signature strategy registry.

## Decision 3 — No shell, ever (security invariant)

Handlers are fork/exec'd as `argv` via `tokio::process::Command`. Payload data is
delivered on **stdin**, never interpolated into a command line. This makes
shell-injection structurally impossible regardless of payload content. A
regression guard test (TASK-446) asserts a payload containing shell
metacharacters (`; rm -rf`, `$(...)`, backticks) reaches the handler verbatim on
stdin and is never executed. Any future change that routes handler invocation
through a shell must be rejected in review.

## Resolved — handler path resolution

`PluginRegistry::load_dir` resolves a relative `command[0]` (containing a `/`)
against the plugin's own directory at manifest-load time (Option 1 below),
so relative handler scripts work regardless of the broker's cwd. Bare program
names are left for `PATH` lookup; absolute paths are unchanged.

Original options considered for the wiring card (TASK-379 follow-up), for
reference:

1. Resolve `command[0]` to `<plugin_dir>/<command[0]>` at manifest-load time when
   the path is relative and the file exists under the plugin dir (preferred —
   keeps manifests portable). **Implemented.**
2. Set `Command::current_dir(plugin_dir)` per handler. Not implemented.

## Consequences

- The GitHub reference plugin (`plugins/github/`) is the executable specimen of
  this contract and doubles as the E2E acceptance fixture (TASK-389).
- Reconciliation cards become pure adoption/deletion (adopt crate verify, adopt
  `run_handler`, delete the `/health` duplicate, remove the schema fork) rather
  than net-new engineering.
- Multi-source signature routing is explicitly deferred with a written backlog
  item, so the sprint isn't blocked on it.

## Alternatives considered

- **Rebuild dispatch in aish core** — rejected; the crate implementation already
  handles concurrency, isolation, timeouts, and audit. Duplication would diverge.
- **Shell-string `command`** — rejected on security grounds (Decision 3).
- **Per-source plugins with hard-coded validators** — deferred; a `source`
  discriminator on the envelope is the cleaner seam.
