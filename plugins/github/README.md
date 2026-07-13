# GitHub Webhook Plugin

Reference webhook plugin for aish (SPR-069, TASK-386 scaffold + TASK-387 handler
scripts). It demonstrates the **canonical** `plugin.json` webhook schema that the
`aish-webhook-client` dispatcher consumes, and ships three self-contained handler
scripts that turn GitHub events into concise, auditable one-line summaries.

## What it does

| GitHub event   | Filter(s)                                         | Handler                    | Emits |
|----------------|---------------------------------------------------|----------------------------|-------|
| `pull_request` | `action ∈ {opened, reopened, ready_for_review}`   | `handlers/pr-review.sh`    | PR triage line (repo#num, author, head→base, title, url) |
| `workflow_run` | `action = completed`                              | `handlers/workflow-run.sh` | CI outcome line (✓/✗, name, branch, status/conclusion); non-zero exit on a bad conclusion; **auto-dispatches a `fix-ci` worker on failure** (see below) |
| `release`      | `action = published`                              | `handlers/release.sh`      | Release notice (tag, name, author, pre-release flag) |

Each `action` value is registered as its own handler entry because filters are
**AND-combined equality** checks — there is no `in` operator, so one entry per
accepted value keeps the match explicit.

## CI auto-fix worker

When a `workflow_run` concludes in a bad state (`failure`, `timed_out`,
`cancelled`, `startup_failure`) **and** the event carries an associated PR (or a
usable head branch), `workflow-run.sh` detaches a background aish coordinator
that runs the [`fix-ci`](../../skills/fix-ci/SKILL.md) skill against the failed
run — it checks out the branch, inspects `gh run view <id> --log-failed`, applies
the smallest correct fix, reconfirms the test/lint gate is green, and pushes to
the **PR branch** (never the default branch).

Properties:

| Property      | Behavior |
|---------------|----------|
| **Opt-out**   | set `GITHUB_CI_AUTOFIX=0` in the handler env to disable (default: enabled) |
| **PR-scoped** | only fires when the payload has `workflow_run.pull_requests[]` or a real `head_branch` |
| **Non-blocking** | worker is `setsid`-detached with closed stdin, so the handler returns well inside its dispatcher timeout |
| **Idempotent** | a per-run marker (`$TMPDIR/aish-ci-autofix-<run_id>.marker`) dedupes webhook redeliveries — one failed run spawns at most one worker |
| **Best-effort** | dispatch failures never change the handler's exit code, which keeps signalling the raw CI conclusion to the audit sink |

The worker's stdout/stderr is captured to `$TMPDIR/ci-autofix-<run_id>-<ts>.log`.
Requires `aish` on `PATH`; if absent, auto-fix is skipped with a logged notice.

## Handler contract

The dispatcher fork/exec's each handler as `argv` — **no shell is ever
involved**, so a payload can never be interpolated into a command line
(shell-injection is structurally impossible; see the regression guard, TASK-446).

Every handler receives:

- **stdin** — the raw GitHub event payload as JSON.
- **env** —
  - `WEBHOOK_ID` — dispatch/delivery id
  - `WEBHOOK_TENANT_ID` — routing tenant
  - `WEBHOOK_PLUGIN_ID` — `github`
  - `WEBHOOK_EVENT_TYPE` — e.g. `pull_request`
- **stdout** — captured and written to the audit sink; keep it to one summary line.
- **exit code** — `0` = success. A non-zero exit is logged + audited as a handler
  failure but **never** blocks sibling handlers (the dispatcher isolates every
  handler and enforces a per-handler timeout).

JSON is parsed with `python3` (ubiquitous, no `jq` dependency). The scripts read
only stdin + env, so they are **location-independent** — they run correctly
regardless of the process cwd the dispatcher launches them under.

## Manifest schema (canonical)

```jsonc
{
  "id": "github",
  "version": "0.1.0",
  "webhooks": [                    // alias "handlers" also accepted for back-compat
    {
      "event_type": "pull_request", // "*" matches all events
      "command": ["handlers/pr-review.sh"], // argv[0] + args; no shell
      "filters": { "action": "opened" },    // AND-combined dotted-path equality
      "timeout_secs": 30            // optional per-handler override
    }
  ]
}
```

## Known limitation — handler path resolution

The current `aish-webhook-client` dispatcher fork/exec's `command[0]` **without
setting the child's cwd or resolving the path relative to the plugin
directory**. A relative `handlers/pr-review.sh` therefore resolves against the
broker process's cwd, not `plugins/github/`. Until core wires plugin-dir
resolution (tracked in the routing ADR, `docs/design/webhook-plugin-routing.md`),
deployments should either run the broker with cwd at the plugin root or rewrite
`command[0]` to an absolute path at load time.

## Local smoke test

```sh
# pull_request
printf '%s' '{"action":"opened","pull_request":{"number":42,"title":"Add widget",
  "user":{"login":"octocat"},"base":{"ref":"main"},"head":{"ref":"feat/widget"},
  "html_url":"https://github.com/acme/repo/pull/42"},
  "repository":{"full_name":"acme/repo"}}' \
  | WEBHOOK_TENANT_ID=t_demo plugins/github/handlers/pr-review.sh
```

Expected:

```
[github/pr] acme/repo#42 opened by @octocat (feat/widget→main) tenant=t_demo: Add widget https://github.com/acme/repo/pull/42
```
