#!/usr/bin/env bash
# SPR-069 / TASK-387 — GitHub workflow_run handler.
#
# Fired on `workflow_run` completed events. Surfaces CI outcome, and returns a
# non-zero exit ONLY as an informational signal on failure/timeout conclusions
# (the dispatcher logs+audits it; sibling handlers are unaffected). stdin = raw
# GitHub `workflow_run` payload JSON; stdout = one summary line.
set -euo pipefail

payload="$(cat)"

python3 - "$payload" <<'PY'
import json, os, sys

raw = sys.argv[1] if len(sys.argv) > 1 else "{}"
try:
    ev = json.loads(raw) if raw.strip() else {}
except json.JSONDecodeError as e:
    print(f"[github/ci] malformed payload: {e}", file=sys.stderr)
    sys.exit(2)

wr = ev.get("workflow_run") or {}
name = wr.get("name") or "?"
status = wr.get("status") or "?"
conclusion = wr.get("conclusion") or "?"
branch = wr.get("head_branch") or "?"
run_num = wr.get("run_number", "?")
url = wr.get("html_url") or ""
repo = ((ev.get("repository") or {}).get("full_name")) or "?"
tenant = os.environ.get("WEBHOOK_TENANT_ID", "-")

bad = {"failure", "timed_out", "cancelled", "startup_failure"}
mark = "\u2717" if conclusion in bad else ("\u2713" if conclusion == "success" else "\u2022")
print(
    f"[github/ci] {mark} {repo} '{name}' run#{run_num} ({branch}) "
    f"{status}/{conclusion} tenant={tenant} {url}".rstrip()
)

# Informational non-zero on a bad CI conclusion so downstream audit can key on it.
sys.exit(1 if conclusion in bad else 0)
PY
