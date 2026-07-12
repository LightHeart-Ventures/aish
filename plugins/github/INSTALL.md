# github Plugin — Installation Guide

This guide walks through installing the **github** plugin, which provides webhook handlers for GitHub events (pull requests, workflow runs, releases). The plugin is lightweight — it has **no external dependencies** — and pairs with the aish webhook broker to ingest GitHub events.

---

## Prerequisites

### What You Need

- **aish:** This plugin is part of the aish ecosystem; you should already have aish running.
- **GitHub repository:** The repo you want to monitor with webhooks.
- **Webhook broker:** A way to receive and route GitHub webhooks (e.g., aish-webhook-broker on Fly.io, or your own HTTP server).
- **GitHub App or Personal Access Token:** (optional, only if handlers invoke `gh` CLI commands).

### No Binary Dependencies

Unlike MCP server plugins (codebase-memory, signoz-observability), this plugin has **no binary dependencies**. The handlers are **POSIX shell scripts** that receive JSON on stdin and emit summaries on stdout.

---

## Prerequisites Setup

### Step 1: Set up webhook ingestion (one-time)

You need a way to **receive GitHub webhooks** and route them to aish. Choose one:

#### Option A — aish-webhook-broker (Recommended)

Deploy the aish webhook broker (typically on Fly.io):

```sh
# See: https://github.com/LightHeart-Ventures/aish-webhook-broker
# or read the skill: :skill add aish/webhook-broker-flyio

fly launch --name my-aish-webhooks --image lightheart-ventures/aish-webhook-broker
fly secrets set WEBHOOK_SECRET="$(openssl rand -hex 32)"
fly deploy
```

Note the broker URL (e.g., `https://my-aish-webhooks.fly.dev`).

#### Option B — Custom HTTP Server

If you have your own webhook receiver:
1. Ensure it can POST events to aish via `curl` or the aish webhook API
2. Configure your GitHub webhook to POST to that server
3. Have the server forward events to aish (see webhook broker code for format)

#### Option C — Local Testing (Development)

For testing on your machine without a public URL:

```sh
# Start a simple local webhook listener
python3 -m http.server 8000
# or use ngrok for a public tunnel to localhost:8000
```

### Step 2: Configure GitHub webhook (per repository)

1. Go to your GitHub repo: **Settings → Webhooks → Add webhook**
2. Set **Payload URL** to your webhook broker/receiver URL:
   ```
   https://my-aish-webhooks.fly.dev/webhooks/github
   ```
3. Set **Content type** to `application/json`
4. Set **Secret** to match your broker's `WEBHOOK_SECRET`
5. Select events to listen for:
   - `pull_request` (for PR triage)
   - `workflow_run` (for CI alerts)
   - `release` (for release notices)
6. Click **Add webhook**

### Step 3: (Optional) Enable `gh` CLI for auto-fix

If you want the `workflow-run.sh` handler to auto-fix failed CI:

```sh
# Install GitHub CLI
brew install gh  # macOS
# or: apt-get install gh  # Linux

# Authenticate
gh auth login
# Choose "GitHub.com" and "HTTPS" and "Paste an authentication token"
# Generate one at https://github.com/settings/tokens (scope: repo, workflow)
```

Then in `~/.aishrc`:

```sh
export GITHUB_TOKEN="<your-token>"  # For gh CLI
export GITHUB_CI_AUTOFIX=1          # Enable auto-fix on workflow failures
```

---

## Verification

### Step 1: Plugin is discovered

From inside aish:

```
:mcp
```

You should see:

```
github (webhook)
  Tools: 3
    - pr-review
    - workflow-run
    - release
```

Or, check that the handlers are executable:

```sh
ls -la ~/.aish/plugins/github/handlers/
# Should show: pr-review.sh, workflow-run.sh, release.sh (all executable)
```

### Step 2: Webhook broker is reachable

Test your webhook broker:

```sh
curl -s https://my-aish-webhooks.fly.dev/health | jq .
# Expected: {"status":"ok"} or similar
```

### Step 3: GitHub webhook is configured

In your repo: **Settings → Webhooks**, click on the webhook you added. You should see:

- **Recent Deliveries** tab shows successful POST attempts (status 200)
- If failed, click the delivery to see the error response

### Step 4: Trigger a test webhook

1. Create a pull request in your repo (or mark an existing one as "ready for review")
2. Check the webhook **Recent Deliveries** — a new entry should appear
3. If successful, you should see a summary in your webhook broker's logs

Example log output:

```
[2024-01-10 12:34:56] github:pull_request { "action": "opened", "pull_request": { "number": 42, "title": "Add feature X", ... } }
[2024-01-10 12:34:56] handler: pr-review.sh → repo#42 by alice opened: Add feature X
```

### Step 5: Load the skill (optional)

```
:skill add github/github
```

This provides usage guidance for the handlers and configuration.

---

## Installation Summary

| Step | Action | Expected Outcome |
|------|--------|-----------------|
| 1. Deploy webhook broker | `fly launch --name my-webhooks ...` | Broker URL: `https://my-webhooks.fly.dev` |
| 2. Add GitHub webhook | Settings → Webhooks → Add webhook | Webhook appears in list, status 200 in Recent Deliveries |
| 3. (Optional) Install `gh` CLI | `brew install gh && gh auth login` | `which gh`, `gh pr list` works |
| 4. (Optional) Set env vars | `export GITHUB_TOKEN="..." GITHUB_CI_AUTOFIX=1` | Env vars in `~/.aishrc` |
| 5. Check plugin | `:mcp` | github appears with 3 handlers |
| 6. Test PR event | Create a PR or push to repo | Handler fires, summary appears in logs |

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `:mcp` doesn't show `github` | Plugin not discovered | Restart aish: `:restart`, or check `~/.aish/plugins/github/` exists |
| Webhook shows 400/500 in Recent Deliveries | Payload malformed or handler crashed | Check webhook broker logs for error message |
| Handler emits no output | Webhook not reaching broker | Check GitHub webhook payload URL is correct and broker is running |
| "No such file or directory" in broker logs | Handler script is not executable or missing | `chmod +x ~/.aish/plugins/github/handlers/*.sh` |
| CI auto-fix doesn't trigger | `gh` not on PATH or `GITHUB_CI_AUTOFIX=0` | `which gh`, check env vars: `echo $GITHUB_CI_AUTOFIX` |
| "403 Forbidden" from `gh` commands | GitHub token expired or insufficient scope | Re-run `gh auth login`, ensure token has `repo` and `workflow` scopes |

---

## Configuration

### Environment Variables

```sh
# Required for CI auto-fix feature
export GITHUB_TOKEN="ghp_..."                  # GitHub Personal Access Token (scope: repo, workflow)

# Optional
export GITHUB_CI_AUTOFIX=1                     # 0 to disable auto-fix (default: enabled)
export WEBHOOK_BROKER_URL="https://..."        # Broker URL (if auto-fix needs to report back)
```

### Handler Configuration

Edit `~/.aish/plugins/github/plugin.json` to customize which events trigger handlers:

```json
{
  "webhooks": [
    {
      "event_type": "pull_request",
      "command": ["handlers/pr-review.sh"],
      "filters": { "action": "opened" },
      "timeout_secs": 30
    },
    {
      "event_type": "workflow_run",
      "command": ["handlers/workflow-run.sh"],
      "filters": { "action": "completed" },
      "timeout_secs": 20
    },
    {
      "event_type": "release",
      "command": ["handlers/release.sh"],
      "filters": { "action": "published" },
      "timeout_secs": 20
    }
  ]
}
```

**Available event_type values:**
- `pull_request` — When a PR is opened, reopened, or marked ready for review
- `workflow_run` — When a GitHub Actions workflow completes
- `release` — When a release is published
- (Add more as needed)

---

## How Handlers Work

Each handler is a **POSIX shell script** that:

1. **Receives:** GitHub event JSON on stdin
2. **Reads:** Environment variables (`WEBHOOK_ID`, `WEBHOOK_EVENT_TYPE`, etc.)
3. **Emits:** A concise one-line summary on stdout
4. **Exits:** 0 on success, non-zero on failure (logged but non-blocking)

**Important:** No shell interpolation happens. The webhook broker fork/execs each handler as `argv`, so payloads can never be interpolated into commands (shell injection is structurally impossible).

### Example Handler Flow

```
GitHub webhook POST
    ↓
aish-webhook-broker receives JSON
    ↓
Broker fork/exec: handlers/pr-review.sh
    ↓
Handler reads stdin (JSON)
    ↓
Handler parses pull_request.number, .title, .user.login
    ↓
Handler emits: "repo#42 by alice opened: Add feature X"
    ↓
Broker captures stdout, writes to audit log
    ↓
Handler exits 0
```

---

## Next Steps

1. **Read the README:**
   ```
   cat ~/.aish/plugins/github/README.md
   ```

2. **Understand handler scripts:**
   - `pr-review.sh` — Parses PR events, emits triage summary
   - `workflow-run.sh` — Parses CI events, spawns auto-fix worker on failure
   - `release.sh` — Parses release events, emits notice

3. **Customize handlers:**
   - Edit handler scripts in `~/.aish/plugins/github/handlers/` to change output format
   - Add filters to `plugin.json` to trigger handlers on different events

4. **Integrate with workflows:**
   - Have handlers POST to your team's Slack/Discord
   - Have workflow-run handler trigger auto-fix background workers
   - Archive summaries to a log aggregator (SigNoz, Datadog, etc.)

5. **Monitor webhook health:**
   - Check GitHub repo Webhooks → Recent Deliveries regularly
   - Monitor broker's error rates and latency
   - Set up an alert if deliveries start failing

6. **Report issues:**
   - aish: https://github.com/LightHeart-Ventures/aish/issues
   - GitHub: https://github.com/github/gh-ost/issues (if using `gh` CLI)

---

## Links

- **Plugin repo:** This directory (`~/.aish/plugins/github/`)
- **Webhook broker:** https://github.com/LightHeart-Ventures/aish-webhook-broker
- **aish webhook skill:** `:skill add aish/webhook-broker-flyio`
- **GitHub webhooks docs:** https://docs.github.com/en/developers/webhooks-and-events/webhooks
- **GitHub API:** https://docs.github.com/en/rest
- **aish docs:** https://github.com/LightHeart-Ventures/aish
