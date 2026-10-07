#!/usr/bin/env bash
# TASK-373 (SPR-104) — GitHub push handler.
#
# Fired on every `push` event (branch or tag; including creations, force-pushes
# and deletions). stdin = raw GitHub `push` payload JSON; stdout = one summary
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
    print(f"[github/push] malformed payload: {e}", file=sys.stderr)
    sys.exit(2)

repo = ((ev.get("repository") or {}).get("full_name")) or "?"
full_ref = ev.get("ref") or "?"
if full_ref.startswith("refs/heads/"):
    kind, ref = "branch", full_ref[len("refs/heads/"):]
elif full_ref.startswith("refs/tags/"):
    kind, ref = "tag", full_ref[len("refs/tags/"):]
else:
    kind, ref = "ref", full_ref

before = (ev.get("before") or "")[:7] or "?"
after = (ev.get("after") or "")[:7] or "?"
commits = ev.get("commits") or []
n = len(commits) if isinstance(commits, list) else 0
pusher = ((ev.get("pusher") or {}).get("name")) or (
    (ev.get("sender") or {}).get("login")
) or "?"
compare = ev.get("compare") or ""
head = ev.get("head_commit") or {}
headline = ((head.get("message") or "").strip().splitlines() or [""])[0]
if len(headline) > 72:
    headline = headline[:72] + "…"
tenant = os.environ.get("WEBHOOK_TENANT_ID", "-")

if ev.get("deleted"):
    print(f"[github/push] {repo} {kind} {ref} deleted by @{pusher} tenant={tenant}")
    sys.exit(0)

flags = ""
if ev.get("forced"):
    flags += " [forced]"
if ev.get("created"):
    flags += " [new]"
noun = "commit" if n == 1 else "commits"
print(
    f"[github/push] {repo} {kind} {ref}: {n} {noun} {before}..{after} "
    f"by @{pusher}{flags} tenant={tenant}: {headline} {compare}".rstrip()
)
PY
