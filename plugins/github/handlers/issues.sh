#!/usr/bin/env bash
# TASK-373 (SPR-104) — GitHub issues handler.
#
# Fired on `issues` events whose action is opened / reopened / edited / closed /
# labeled (one manifest entry per action — dispatcher filters are AND-combined
# equality). stdin = raw GitHub `issues` payload JSON; stdout = one summary
# line. Location-independent (stdin + env only).
#
# Shell-injection guard: the payload is passed to python3 as a single argv
# element via a QUOTED heredoc; nothing from the payload is ever interpolated
# into a shell command line or eval'd.
set -euo pipefail

payload="$(cat)"

python3 - "$payload" <<'PY'
import json, os, sys

raw = sys.argv[1] if len(sys.argv) > 1 else "{}"
try:
    ev = json.loads(raw) if raw.strip() else {}
except json.JSONDecodeError as e:
    print(f"[github/issue] malformed payload: {e}", file=sys.stderr)
    sys.exit(2)

issue = ev.get("issue") or {}
repo = ((ev.get("repository") or {}).get("full_name")) or "?"
action = ev.get("action") or "?"
num = issue.get("number", "?")
title = (issue.get("title") or "").strip()
author = ((issue.get("user") or {}).get("login")) or "?"
state = issue.get("state") or "?"
url = issue.get("html_url") or ""
labels = [
    (l or {}).get("name") or ""
    for l in (issue.get("labels") or [])
    if isinstance(l, dict)
]
labels = ",".join(x for x in labels if x) or "-"
tenant = os.environ.get("WEBHOOK_TENANT_ID", "-")

extra = ""
if action in ("labeled", "unlabeled"):
    lname = (ev.get("label") or {}).get("name")
    if lname:
        extra += f" {'+' if action == 'labeled' else '-'}label:{lname}"
if "pull_request" in issue:
    extra += " [pr]"

print(
    f"[github/issue] {repo}#{num} {action}{extra} by @{author} [{state}] "
    f"labels={labels} tenant={tenant}: {title} {url}".rstrip()
)
PY
