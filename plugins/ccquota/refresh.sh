#!/usr/bin/env bash
#
# refresh.sh — throttled TurnEnd refresher for the ccquota statusline badge.
#
# Wired via hooks.json on the interactive-agent TurnEnd event. Because driving
# Claude Code through tmux takes several seconds, this must NEVER block the
# REPL, and must NOT run on every turn. Design:
#   1. Throttle on a stamp file: if the last refresh was < throttle_seconds ago,
#      exit 0 immediately (the common path — microseconds).
#   2. When due, touch the stamp FIRST (so concurrent/next turns stay throttled),
#      then spawn the actual cclimits.sh capture DETACHED and return 0 at once.
#   3. The detached worker writes a one-line badge to
#      ~/.aish/state/statusline/ccquota.txt, which aish's file-backed statusline
#      reader (TASK-316) folds onto the SecondStatusLine. Core hides the segment
#      once its mtime goes stale (> 1h), so a wedged capture self-heals.
#
# Always exits 0 — a status badge must never fail a turn.

set -u

THROTTLE_SECONDS="${CCQUOTA_THROTTLE_SECONDS:-600}"

PLUGIN_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CCLIMITS="$PLUGIN_DIR/cclimits.sh"
STATE_DIR="${HOME:-/tmp}/.aish/state"
SEG_DIR="$STATE_DIR/statusline"
STAMP="$STATE_DIR/ccquota.stamp"
SEG_FILE="$SEG_DIR/ccquota.txt"
LOCK="$STATE_DIR/ccquota.lock"

mkdir -p "$SEG_DIR" 2>/dev/null || exit 0

# --- Throttle ---
now=$(date +%s)
if [[ -f "$STAMP" ]]; then
  last=$(cat "$STAMP" 2>/dev/null || echo 0)
  case "$last" in ''|*[!0-9]*) last=0 ;; esac
  if (( now - last < THROTTLE_SECONDS )); then
    exit 0
  fi
fi

# Best-effort single-flight: bail if another refresh is mid-capture.
if [[ -d "$LOCK" ]]; then
  # Stale lock (> 5 min) → reclaim.
  if [[ -n "$(find "$LOCK" -maxdepth 0 -mmin +5 2>/dev/null)" ]]; then
    rmdir "$LOCK" 2>/dev/null || true
  else
    exit 0
  fi
fi

# Claim the throttle window immediately so the next turn doesn't re-trigger.
echo "$now" > "$STAMP" 2>/dev/null || true

[[ -x "$CCLIMITS" ]] || exit 0

# --- Detached capture -------------------------------------------------------
# Do the slow tmux work off the hook's critical path. setsid when available so
# it fully detaches from the REPL's process group; fall back to a backgrounded
# subshell otherwise.
run_capture() {
  mkdir "$LOCK" 2>/dev/null || exit 0
  trap 'rmdir "$LOCK" 2>/dev/null || true' EXIT

  json="$("$CCLIMITS" --json 2>/dev/null)" || exit 0
  [[ -z "$json" ]] && exit 0

  badge=""
  if command -v python3 >/dev/null 2>&1; then
    badge="$(printf '%s' "$json" | python3 "$PLUGIN_DIR/badge.py" 2>/dev/null)" || badge=""
  fi

  # Only overwrite the segment when we produced a non-empty badge; otherwise
  # leave the previous one to age out naturally.
  [[ -n "$badge" ]] && printf '%s\n' "$badge" > "$SEG_FILE" 2>/dev/null || true
}

if command -v setsid >/dev/null 2>&1; then
  setsid bash -c "$(declare -f run_capture); run_capture" </dev/null >/dev/null 2>&1 &
else
  ( run_capture </dev/null >/dev/null 2>&1 & )
fi

exit 0
