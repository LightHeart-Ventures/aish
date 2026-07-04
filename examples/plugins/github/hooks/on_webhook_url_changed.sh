#!/usr/bin/env bash
# github plugin — on_webhook_url_changed lifecycle hook.
#
# Fires whenever aish's public webhook-broker URL changes (broker restart,
# tunnel re-provision, first assignment). aish exports the new URL as
# $AISH_WEBHOOK_URL. A production plugin would call the GitHub API here to
# PATCH the repo/org webhook config so deliveries keep flowing to the new URL.
#
# CONTRACT
#   * $AISH_WEBHOOK_URL   the new public base URL (may be empty when torn down)
#   * $GITHUB_OWNER/$GITHUB_REPO  target repo (from on_init injection)
#   * STDERR/STDIN inherited; exit non-zero to signal failure (logged, non-fatal)
set -euo pipefail

new_url="${AISH_WEBHOOK_URL:-}"
owner="${GITHUB_OWNER:-}"
repo="${GITHUB_REPO:-}"

if [[ -z "${new_url}" ]]; then
  echo "github: webhook URL cleared — deliveries paused until reassigned" >&2
  exit 0
fi

echo "github: webhook URL changed → ${new_url}" >&2
echo "github: would PATCH https://api.github.com/repos/${owner}/${repo}/hooks to point at ${new_url}/github" >&2

# Real implementation (requires [profile:github] token, omitted from the demo):
#   gh api -X PATCH "repos/${owner}/${repo}/hooks/${HOOK_ID}" \
#     -f "config[url]=${new_url}/github" -f "config[content_type]=json"
exit 0
