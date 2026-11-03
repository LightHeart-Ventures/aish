#!/usr/bin/env sh
# cua-repl.sh — launch the trycua/cua computer-use REPL with aish-friendly defaults.
#
# Usage:
#   bin/cua-repl.sh                                  # interactive, default model+provider
#   bin/cua-repl.sh anthropic/claude-sonnet-4-5-20250929
#   bin/cua-repl.sh openai/computer-use-preview -p "open Safari and search for cua"
#   bin/cua-repl.sh --predict-click "the Send button"
#
# Everything after the (optional) leading model string is passed through verbatim
# to `python -m cua_agent.cli`.
#
# Env (all optional; normally supplied by plugin config):
#   CUA_PYTHON          interpreter with cua-agent installed        (default python3)
#   CUA_MODEL           default model string
#   CUA_PROVIDER        cloud | lume | winsandbox | docker          (default cloud)
#   CUA_CONTAINER_NAME  sandbox name (stops the REPL prompting)
#   CUA_API_KEY         Cua cloud key — required for provider=cloud
#   CUA_BUDGET_USD      per-session spend cap -> --budget
#   CUA_IMAGES          recent screenshots in context -> --images
#   CUA_SHOW_USAGE      1 -> -u
#   CUA_TRAJECTORY      1 -> --trajectory
#   CUA_REPL_ARGS       raw extra flags appended to every run
set -eu

PY="${CUA_PYTHON:-python3}"
PROVIDER="${CUA_PROVIDER:-cloud}"
MODEL="${CUA_MODEL:-anthropic/claude-sonnet-4-5-20250929}"

# A leading bare word (no dash) overrides the configured model.
case "${1:-}" in
  -*|"") : ;;
  *) MODEL="$1"; shift ;;
esac

die() { printf 'cua-repl: %s\n' "$1" >&2; exit "${2:-1}"; }

command -v "$PY" >/dev/null 2>&1 || die "python interpreter '$PY' not found — set CUA_PYTHON" 127

"$PY" - <<'PY' >/dev/null 2>&1 || die "cua-agent is not importable by '$PY' — fix: $PY -m pip install 'cua-agent[cli,computer]'" 69
import cua_agent  # noqa: F401
import cua_agent.cli  # noqa: F401  (needs the [cli] extra: yaspin)
PY

if [ "$PROVIDER" = cloud ] && [ -z "${CUA_API_KEY:-}" ]; then
  die "CUA_API_KEY is unset but provider=cloud — export it from your credentials profile, or pick a local provider (--provider docker|lume|winsandbox)" 78
fi

set -- "$MODEL" --provider "$PROVIDER" "$@"

[ -n "${CUA_IMAGES:-}" ] && set -- "$@" --images "$CUA_IMAGES"
[ -n "${CUA_BUDGET_USD:-}" ] && set -- "$@" --budget "$CUA_BUDGET_USD"
[ "${CUA_SHOW_USAGE:-0}" = 1 ] && set -- "$@" --usage
[ "${CUA_TRAJECTORY:-0}" = 1 ] && set -- "$@" --trajectory

# shellcheck disable=SC2086  # intentional word-splitting of raw extra flags
exec "$PY" -m cua_agent.cli "$@" ${CUA_REPL_ARGS:-}
