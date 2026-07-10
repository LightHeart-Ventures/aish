#!/usr/bin/env bash
# SPR-069 / TASK-387 — GitHub release handler.
#
# Fired on `release` published events. stdin = raw GitHub `release` payload JSON;
# stdout = one summary line. Location-independent (stdin + env only).
set -euo pipefail

payload="$(cat)"

python3 - "$payload" <<'PY'
import json, os, sys

raw = sys.argv[1] if len(sys.argv) > 1 else "{}"
try:
    ev = json.loads(raw) if raw.strip() else {}
except json.JSONDecodeError as e:
    print(f"[github/release] malformed payload: {e}", file=sys.stderr)
    sys.exit(2)

rel = ev.get("release") or {}
tag = rel.get("tag_name") or "?"
name = (rel.get("name") or "").strip()
author = ((rel.get("author") or {}).get("login")) or "?"
prerelease = rel.get("prerelease", False)
url = rel.get("html_url") or ""
repo = ((ev.get("repository") or {}).get("full_name")) or "?"
tenant = os.environ.get("WEBHOOK_TENANT_ID", "-")

kind = "pre-release" if prerelease else "release"
label = f" {name}" if name and name != tag else ""
print(
    f"[github/release] {repo} {kind} {tag}{label} by @{author} "
    f"tenant={tenant} {url}".rstrip()
)
PY
