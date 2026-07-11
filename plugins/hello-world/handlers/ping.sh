#!/usr/bin/env bash
# hello-world ping handler — processes ping webhook events.
# Receives raw JSON on stdin, outputs greeting to aish statusline.
set -euo pipefail

python3 - <<'PY'
import json, os, sys

payload = sys.stdin.read()
try:
    ev = json.loads(payload) if payload.strip() else {}
except json.JSONDecodeError as e:
    print(f"[hello-world/ping] malformed payload: {e}", file=sys.stderr)
    sys.exit(2)

message = ev.get("message", "Hello, World!")

# Output to aish statusline
statusline_msg = f"👋 {message}"
print(statusline_msg)
PY
