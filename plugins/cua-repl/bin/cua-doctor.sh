#!/usr/bin/env sh
# cua-doctor.sh — preflight the Cua computer-use REPL. Exits 0 when the REPL can
# start, 1 when a FAIL was recorded. Prints one `status  check  detail` line per
# check so the agent can read it without parsing prose.
set -u

PY="${CUA_PYTHON:-python3}"
PROVIDER="${CUA_PROVIDER:-cloud}"
MODEL="${CUA_MODEL:-anthropic/claude-sonnet-4-5-20250929}"
FAILED=0

say() { printf '%-5s  %-22s  %s\n' "$1" "$2" "$3"; }
ok()   { say OK   "$1" "$2"; }
warn() { say WARN "$1" "$2"; }
bad()  { say FAIL "$1" "$2"; FAILED=1; }

if command -v "$PY" >/dev/null 2>&1; then
  ok python "$("$PY" -V 2>&1) at $(command -v "$PY")"
  "$PY" -c 'import sys; raise SystemExit(0 if (3,11) <= sys.version_info < (3,14) else 1)' 2>/dev/null \
    || warn python-version "cua-agent requires >=3.11,<3.14"
else
  bad python "'$PY' not found — set CUA_PYTHON"
fi

if [ "$FAILED" -eq 0 ]; then
  if "$PY" -c 'import cua_agent' >/dev/null 2>&1; then
    ok cua-agent "$("$PY" -c 'import cua_agent,sys; print(getattr(cua_agent,"__version__","?"))' 2>/dev/null)"
  else
    bad cua-agent "not importable — $PY -m pip install 'cua-agent[cli,computer]'"
  fi

  if "$PY" -c 'import yaspin' >/dev/null 2>&1; then
    ok cli-extra "yaspin present"
  else
    bad cli-extra "missing [cli] extra — $PY -m pip install 'cua-agent[cli]'"
  fi

  if "$PY" -c 'import cua_sandbox' >/dev/null 2>&1; then
    ok cua-sandbox "present (computer control available)"
  else
    bad cua-sandbox "missing [computer] extra — $PY -m pip install 'cua-agent[computer]'"
  fi
fi

case "$PROVIDER" in
  cloud)
    [ -n "${CUA_API_KEY:-}" ] && ok cua-api-key "set" \
      || bad cua-api-key "unset — required for provider=cloud (https://cua.ai)"
    [ -n "${CUA_CONTAINER_NAME:-}" ] && ok container "$CUA_CONTAINER_NAME" \
      || warn container "CUA_CONTAINER_NAME unset — the REPL will prompt on stdin"
    ;;
  docker)
    command -v docker >/dev/null 2>&1 && ok docker "$(docker --version 2>&1 | head -n1)" \
      || bad docker "not found — required for --provider docker"
    ;;
  lume)
    command -v lume >/dev/null 2>&1 && ok lume "present" \
      || warn lume "lume CLI not found — macOS VM provider may fail"
    ;;
  winsandbox)
    warn winsandbox "Windows Sandbox must be enabled on the host"
    ;;
  *)
    bad provider "unknown provider '$PROVIDER' (cloud|lume|winsandbox|docker)"
    ;;
esac

case "$MODEL" in
  *anthropic/*)
    [ -n "${ANTHROPIC_API_KEY:-}" ] && ok model-key "ANTHROPIC_API_KEY set" \
      || bad model-key "ANTHROPIC_API_KEY unset for model $MODEL"
    ;;
  *openai/*)
    [ -n "${OPENAI_API_KEY:-}" ] && ok model-key "OPENAI_API_KEY set" \
      || bad model-key "OPENAI_API_KEY unset for model $MODEL"
    ;;
  *) warn model-key "no key check for model $MODEL (local or third-party loop)" ;;
esac

[ -n "${CUA_BUDGET_USD:-}" ] && ok budget "--budget \$$CUA_BUDGET_USD" \
  || warn budget "no spend cap configured — set budget_usd"

exit "$FAILED"
