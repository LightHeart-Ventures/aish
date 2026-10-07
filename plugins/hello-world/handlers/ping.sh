#!/usr/bin/env bash
# hello-world ping handler — processes `ping` webhook events.
# Receives the raw JSON payload on stdin; its stdout is flashed on the aish
# SecondStatusLine (first non-blank line, capped).
set -euo pipefail

# Read the payload BEFORE invoking python: `python3 - <<PY` makes the heredoc
# python's stdin, so reading sys.stdin inside the script would see nothing.
payload="$(cat)"

AISH_PING_PAYLOAD="$payload" python3 - <<'PY'
import json, os, sys

payload = os.environ.get("AISH_PING_PAYLOAD", "")
try:
    ev = json.loads(payload) if payload.strip() else {}
except json.JSONDecodeError as e:
    print(f"[hello-world/ping] malformed payload: {e}", file=sys.stderr)
    sys.exit(2)

message = ev.get("message") if isinstance(ev, dict) else None
print(f"👋 {message or 'Hello, World!'}")
PY
