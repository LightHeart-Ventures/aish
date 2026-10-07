# GitHub Webhook Plugin

Turns GitHub events (pushes, PRs, issues, CI runs, releases) into one-line
SecondStatusLine flashes in your aish session, and can optionally launch aish
agents to review PRs and fix failing CI. Events reach aish through the
[webhook broker](../../docs/webhooks.md); handlers follow the standard
[webhook handler contract](../../docs/WEBHOOK_HANDLERS.md).

- [Quick start](#quick-start)
- [What it does](#what-it-does)
- [Environment flags](#environment-flags)
- [Troubleshooting](#troubleshooting)

## Quick start

### 1. Prerequisites

| Need | Why |
|---|---|
| `bash`, `python3` | every handler parses the payload with Python (no `jq`) |
| A reachable `aish-webhook-broker` | receives GitHub's POSTs and relays them to aish — see [DEPLOYMENT.md](../../crates/aish-webhook-broker/docs/DEPLOYMENT.md) |
| `aish` on `PATH` | only for the PR-review and CI auto-fix agents |
| `gh` authenticated | only for the agents: `gh auth login`, or `GITHUB_TOKEN` in aish's env |

### 2. Install the plugin

```sh
cp -r plugins/github ~/.aish/plugins/github
chmod +x ~/.aish/plugins/github/handlers/*.sh
```

In aish: `:plugin info github` should list the `webhooks` handlers and
`:plugin list` should show `github … (ok)`.

### 3. Connect aish to the broker

Pick a tenant id (any string; `default` if you skip it) and a shared secret:

```sh
export WEBHOOK_BROKER_URL=wss://<your-broker>/ws
export WEBHOOK_PLUGIN_ID=github
export WEBHOOK_TENANT_ID=acme                  # optional, default "default"
export WEBHOOK_BROKER_SECRET=$(openssl rand -hex 32)   # store it; reuse below
aish
```

`:webhook status` should report `connected`. aish must have connected at least
once before GitHub delivers: the broker returns `404` for a route nobody has
registered.

### 4. Configure the GitHub webhook

Repository (or organization) → **Settings → Webhooks → Add webhook**:

| Field | Value |
|---|---|
| Payload URL | `https://<your-broker>/webhooks/<tenant>/github` (e.g. `…/webhooks/acme/github`) |
| Content type | `application/json` |
| Secret | the exact `WEBHOOK_BROKER_SECRET` value |
| Events | *Let me select*: **Pushes, Pull requests, Issues, Workflow runs, Releases** |

GitHub sends `X-GitHub-Event` (the broker uses it as the event type) and
`X-Hub-Signature-256` (verified by the broker against the secret). GitHub's
initial `ping` delivery is accepted and logged but no `github` handler matches
it — that's expected.

### 5. Authentication for the agents

The handlers themselves never call GitHub. The optional agents they launch use
`gh`, so the account `gh` is logged in as needs:

| Agent | Permission |
|---|---|
| PR review (`GITHUB_PR_AUTOREVIEW=1`) | Pull requests: read & write (to post a review comment) |
| CI auto-fix (default on) | Contents: read & write, Actions: read (to read logs and push to the PR branch) |

A classic PAT needs `repo` + `workflow`. Prefer a fine-grained token scoped to
the repositories you route here, exported as `GITHUB_TOKEN` before starting aish.

## What it does

| GitHub event   | Filter(s)                                         | Handler                    | Emits |
|----------------|---------------------------------------------------|----------------------------|-------|
| `push`         | none (every branch/tag push, incl. create/delete/force) | `handlers/push.sh`   | Push line (repo, branch/tag, commit count, before..after short SHAs, pusher, `[forced]`/`[new]`, head-commit headline, compare url); a `deleted` line for ref deletions |
| `pull_request` | `action ∈ {opened, reopened, ready_for_review}`   | `handlers/pr-review.sh`    | PR triage line (repo#num, author, head→base, title, url); **opt-in review agent** (see below) |
| `issues`       | `action ∈ {opened, reopened, edited, closed, labeled}` | `handlers/issues.sh`  | Issue line (repo#num, action, `+label:<name>` on labeled, `[pr]` for PR-backed issues, author, state, labels, title, url) |
| `workflow_run` | `action = completed`                              | `handlers/workflow-run.sh` | CI outcome line (✓/✗, name, branch, status/conclusion); non-zero exit on a bad conclusion; **auto-dispatches a CI-fix worker on failure** (see below) |
| `release`      | `action = published`                              | `handlers/release.sh`      | Release notice (tag, name, author, pre-release flag) |

Each `action` value is registered as its own handler entry because filters are
**AND-combined equality** checks — there is no `in` operator, so one entry per
accepted value keeps the match explicit.

Example lines:

```
[github/push] acme/widgets branch main: 3 commits 6113728..0d1a26e by @octocat tenant=t1: Fix widget lookup off-by-one https://github.com/acme/widgets/compare/6113728f27ae...0d1a26e67d8f
[github/issue] acme/widgets#1347 opened by @octocat [open] labels=bug,registry tenant=t1: Widget registry panics on empty config https://github.com/acme/widgets/issues/1347
```

## Environment flags

| Variable               | Default | Effect |
|------------------------|---------|--------|
| `GITHUB_PR_AUTOREVIEW` | off     | `1` (exactly) → `pr-review.sh` dispatches a background review agent on PR open (below). Any other value or unset = off. |
| `GITHUB_CI_AUTOFIX`    | on      | `0` → `workflow-run.sh` does NOT dispatch the CI-fix worker on a failed run. |

Set them in the environment of the process running the webhook client; handlers
inherit it.

## PR review agent (opt-in)

With `GITHUB_PR_AUTOREVIEW=1`, when a **non-draft** PR is `opened`, `reopened`
or `ready_for_review`, `pr-review.sh` detaches a background aish coordinator
(`aish --coordinator --run-id pr-review-<num>-<sha7>-<ts> -c "<task>"`) that
reads the diff with `gh pr diff`, reviews it, and posts findings as a single
`gh pr review --comment`. It never pushes, approves or merges.

| Property      | Behavior |
|---------------|----------|
| **Opt-in**    | off by default — every review spends model tokens |
| **Drafts**    | skipped (`ready_for_review` fires it once the PR is marked ready) |
| **Non-blocking** | `setsid`-detached (falls back to `nohup` where `setsid` is absent, e.g. macOS), closed stdin |
| **Idempotent** | one marker per (repo, PR, head SHA): `$TMPDIR/aish-pr-autoreview-<repo>-<num>-<sha>.marker` — redeliveries and opened+ready_for_review on the same SHA spawn one reviewer; a new head SHA (e.g. reopened after a push) gets a fresh review |
| **Best-effort** | missing `aish` on `PATH` → logged skip; dispatch never changes the handler exit code or its stdout summary |

The reviewer's output goes to `$TMPDIR/pr-review-<num>-<sha7>-<ts>.log`.

## CI auto-fix worker

When a `workflow_run` concludes in a bad state (`failure`, `timed_out`,
`cancelled`, `startup_failure`) **and** the event carries an associated PR (or a
usable head branch), `workflow-run.sh` detaches a background aish coordinator
with self-contained instructions (no skill dependency) to fix the failed run —
it checks out the branch, inspects `gh run view <id> --repo <repo> --log-failed`,
finds the root cause, applies the smallest correct fix, reconfirms the project's
test/lint gate is green, and pushes to the **PR branch** (never the default
branch, never force-push).

Both dispatched agents (this one and the PR reviewer below) get their full
procedure inline in the `-c` prompt. They do not name a skill, because none
ships with this plugin. The coordinator's skill catalog is only
`~/.aish/skills` plus installed plugins' `skills/`, so a named skill could be
missing on the host.

Properties:

| Property      | Behavior |
|---------------|----------|
| **Opt-out**   | set `GITHUB_CI_AUTOFIX=0` in the handler env to disable (default: enabled) |
| **PR-scoped** | only fires when the payload has `workflow_run.pull_requests[]` or a real `head_branch` |
| **Non-blocking** | worker is `setsid`-detached (falls back to `nohup` where `setsid` is absent, e.g. macOS) with closed stdin, so the handler returns well inside its dispatcher timeout |
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
  "version": "0.2.0",
  "webhooks": [                    // canonical key; legacy top-level "handlers" is rejected (TASK-447)
    {
      "event_type": "pull_request", // "*" matches all events
      "command": ["handlers/pr-review.sh"], // argv[0] + args; no shell
      "filters": { "action": "opened" },    // AND-combined dotted-path equality
      "timeout_secs": 30            // optional per-handler override
    }
  ]
}
```

## Handler path resolution

`PluginRegistry::load_dir` resolves a relative `command[0]` (one containing a
`/`, e.g. `handlers/pr-review.sh`) against the plugin's own directory at
manifest-load time, so `plugins/github/handlers/pr-review.sh` runs correctly
regardless of the broker process's cwd. Bare program names (no `/`) are left
alone for `PATH` lookup, and absolute paths are unchanged. See
`crates/aish-webhook-client/src/dispatcher.rs` (`PluginRegistry::load_dir`).

## Tests and fixtures

Recorded-shape GitHub payloads live in `plugins/github/tests/fixtures/`
(`push*.json`, `issues-*.json`, `pull_request-*.json`).
`crates/aish-webhook-client/tests/github_plugin_fixtures.rs` loads this plugin
with `PluginRegistry::load_dir`, dispatches each fixture through the real
`WebhookDispatcher` (fork/exec, no shell) and asserts on the summary lines. It
also proves shell metacharacters in payload fields are printed verbatim and never
executed, and drives the `GITHUB_PR_AUTOREVIEW` path against a stub `aish` on
`PATH` (off by default, once per head SHA, drafts skipped).

```sh
cargo test -p aish-webhook-client --no-default-features --test github_plugin_fixtures
shellcheck plugins/github/handlers/*.sh
```

For a live check, use GitHub's **Recent Deliveries → Redeliver** and watch
`:webhook logs --plugin github`; or replay a logged delivery locally with
`:webhook replay github last --run`.

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

```sh
# push / issues, from the recorded fixtures
WEBHOOK_TENANT_ID=t_demo plugins/github/handlers/push.sh   < plugins/github/tests/fixtures/push.json
WEBHOOK_TENANT_ID=t_demo plugins/github/handlers/issues.sh < plugins/github/tests/fixtures/issues-opened.json
```

```sh
# workflow_run failure (exits 1 by design; set GITHUB_CI_AUTOFIX=0 to avoid spawning a worker)
GITHUB_CI_AUTOFIX=0 WEBHOOK_TENANT_ID=t_demo plugins/github/handlers/workflow-run.sh \
  < plugins/github/tests/fixtures/workflow_run-failure.json
```

From inside aish, without GitHub or a broker:

```text
:webhook test github push --payload plugins/github/tests/fixtures/push.json --run
```

## Logs and state

| What | Where |
|---|---|
| Delivery log (per event, handler status, redacted payload) | `~/.aish/state/webhooks/github.jsonl` — `:webhook logs --plugin github` |
| Handler failures / timeouts | `~/.aish/plugins/github/errors.jsonl` — `:plugin errors github` |
| PR-review agent output | `$TMPDIR/pr-review-<num>-<sha7>-<ts>.log` |
| CI auto-fix agent output | `$TMPDIR/ci-autofix-<run_id>-<ts>.log` |
| Idempotency markers | `$TMPDIR/aish-pr-autoreview-*.marker`, `$TMPDIR/aish-ci-autofix-*.marker` — delete one to allow a re-run |

## Troubleshooting

| Symptom | Fix |
|---|---|
| GitHub "Recent Deliveries" shows **404** | No aish session registered `<tenant>/github`: start aish with `WEBHOOK_PLUGIN_ID=github` and the same `WEBHOOK_TENANT_ID` as the URL. |
| GitHub shows **401** | Secret mismatch: the GitHub webhook secret must equal `WEBHOOK_BROKER_SECRET`. Re-save both. |
| GitHub shows **202** but nothing flashes | `:webhook logs --plugin github`: if the record shows `skipped`, the action isn't one this plugin handles (e.g. `issues` `assigned`, `pull_request` `synchronize`). If there's no record, check `:webhook status`. |
| Handler `error`, exit 127 / permission denied | `chmod +x ~/.aish/plugins/github/handlers/*.sh`; `python3` must be on `PATH`. |
| Handler exit 2, "malformed payload" | Content type isn't `application/json` in the GitHub webhook settings. |
| `workflow_run` always shows `error` for failed runs | By design: the handler exits 1 for `failure`/`timed_out`/`cancelled`/`startup_failure` so the audit reflects CI. |
| No PR review appears | `GITHUB_PR_AUTOREVIEW` must be exactly `1`; drafts are skipped; `aish` must be on `PATH`; check the `$TMPDIR/pr-review-*.log`; a marker for the same head SHA suppresses repeats. |
| CI worker never starts | `GITHUB_CI_AUTOFIX=0` set, or the run has no PR / head branch; see `$TMPDIR/ci-autofix-*.log`. |
| Agents fail to post / push | `gh auth status` in the env aish runs in; token permissions above. |
| Flash shows only the latest event | The flash slot is most-recent-wins; use `:webhook logs` for history. |
