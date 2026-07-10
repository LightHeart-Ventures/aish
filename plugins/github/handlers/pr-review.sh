#!/usr/bin/env bash
# SPR-069 / TASK-387 — GitHub pull_request handler.
#
# Contract (aish-webhook-client dispatcher, no shell in the loop — this script
# is fork/exec'd directly as argv[0]):
#   * stdin   : the raw GitHub `pull_request` event payload as JSON.
#   * env     : WEBHOOK_ID, WEBHOOK_TENANT_ID, WEBHOOK_PLUGIN_ID, WEBHOOK_EVENT_TYPE.
#   * stdout  : a single concise summary line (captured + audited by the dispatcher).
#   * exit 0  : success. Non-zero is logged as a handler failure but never blocks
#               sibling handlers (the dispatcher isolates every handler).
#
# JSON is parsed with python3 (ubiquitous, no jq dependency). The script is
# location-independent: it reads only stdin + env, so it works regardless of the
# process cwd the dispatcher runs it under.
set -euo pipefail

payload="$(cat)"

python3 - "$payload" <<'PY'
import json, os, sys

raw = sys.argv[1] if len(sys.argv) > 1 else "{}"
try:
    ev = json.loads(raw) if raw.strip() else {}
except json.JSONDecodeError as e:
    print(f"[github/pr] malformed payload: {e}", file=sys.stderr)
    sys.exit(2)

pr = ev.get("pull_request") or {}
action = ev.get("action", "?")
num = pr.get("number", "?")
title = (pr.get("title") or "").strip()
author = ((pr.get("user") or {}).get("login")) or "?"
base = ((pr.get("base") or {}).get("ref")) or "?"
head = ((pr.get("head") or {}).get("ref")) or "?"
url = pr.get("html_url") or ""
repo = ((ev.get("repository") or {}).get("full_name")) or "?"
draft = pr.get("draft", False)

tenant = os.environ.get("WEBHOOK_TENANT_ID", "-")
draft_tag = " [draft]" if draft else ""
print(
    f"[github/pr] {repo}#{num} {action}{draft_tag} by @{author} "
    f"({head}\u2192{base}) tenant={tenant}: {title} {url}".rstrip()
)
PY
