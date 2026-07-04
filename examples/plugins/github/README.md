# GitHub Integration — aish plugin

The first **real** aish plugin, and the reference example that exercises *every*
plugin surface at once. If you're building an aish plugin, read this directory
top to bottom — it's the canonical shape.

Part of **SPR-059** (Plugin Ecosystem Phase 1) — the plugin half that consumes
the webhook broker (`crates/webhook-broker`, PR #515) and the handler-dispatch
layer.

## What it does

- Reviews pull requests (`github-pr-review` skill) on `opened` / `synchronize`
  / `ready_for_review`.
- Triages issues (`github-issue-triage` skill) on `opened`.
- Exposes three MCP-backed tools — `create_pr`, `list_issues`, `add_comment` —
  usable both by the skills and directly from the shell.

## Surfaces exercised

| Surface | Where | Purpose |
|---|---|---|
| MCP server | `.mcp.json` | GitHub API access (token from `[profile:github]`) |
| Skills | `skills/*/SKILL.md` | PR review + issue triage playbooks |
| Tools | `tools/*.json` | `create_pr`, `list_issues`, `add_comment` |
| Webhook handlers | `handlers/*.sh` | route `pull_request`, `issues`, `pull_request_review` |
| Lifecycle hooks | `hooks/*.sh` | `on_init`, `on_shell_ready`, `on_webhook_url_changed` |
| Output schemas | `schemas/*.json` | typed `github-pr` / `github-issue` observations |
| Login handler | `login.sh` | OAuth/token bootstrap for the `github` login |
| Plugin memory | `memory: true` | remembers repo + triage state across sessions |

## Layout

```
github/
├── plugin.json        # manifest — declares every surface above
├── config.json        # user-tunable config (owner, repo, labels, reviewers)
├── hooks.json         # webhook + lifecycle → script routing
├── .mcp.json          # GitHub MCP server definition
├── login.sh           # login handler (github)
├── hooks/             # lifecycle scripts
├── handlers/          # one script per webhook event
├── schemas/           # JSON Schemas for emitted observations
├── skills/            # github-pr-review, github-issue-triage
└── tools/             # create_pr, list_issues, add_comment
```

## Configuration

Set via `config.json` (see `config_schema` in `plugin.json` for the full spec):

| Key | Default | Meaning |
|---|---|---|
| `owner` | `${env:GITHUB_OWNER}` | org/user that owns the repo |
| `repo` | `${env:GITHUB_REPO}` | repository name |
| `default_reviewers` | `[]` | logins auto-requested on `create_pr` |
| `triage_labels` | `bug, enhancement, question, needs-triage` | label vocabulary triage may apply |
| `auto_comment_on_open` | `false` | ack-comment on newly opened issues |

The GitHub API token is **never** stored in config — it's resolved at runtime
from the `[profile:github]` credentials section. No secrets on disk.

## Webhook flow

```
GitHub → webhook-broker (SPR-059) → aish client → hooks.json route → handlers/<event>.sh
                                                                          │
                                                          emits typed observation (schemas/)
                                                                          │
                                                              skill fires (review / triage)
```

## Guardrails

- Skills review/triage only — they never merge, close, or force-push.
- Triage applies exactly one label, only from `triage_labels`.
- Security-flavored issues are acknowledged publicly but details go to a
  maintainer privately, never into a public comment.
