#!/usr/bin/env bash
#
# cclimits.sh — read Claude Code subscription usage from `/usage` and emit it,
# optionally as JSON, for a status badge.
#
# VENDORED from dandaka/ccquota (https://github.com/dandaka/ccquota), MIT.
# See ./NOTICE for attribution. Local modifications for aish (SPR-073):
#   * TASK-319: compute_pace() rewritten to be portable across GNU coreutils
#     `date` (Linux) AND BSD `date` (macOS). Upstream used BSD-only `date -j`
#     flags, so pace silently never computed on Linux. It now detects the date
#     flavor once and parses reset timestamps with the matching syntax, and it
#     degrades gracefully (emits no `pace` field) on any parse failure instead
#     of aborting the whole run.
#
# Usage: cclimits.sh [--json]

set -euo pipefail

JSON=false
for arg in "$@"; do
  [[ "$arg" == "--json" ]] && JSON=true
done

# --- Dependency checks ---

TMUX=$(command -v tmux 2>/dev/null) || {
  echo "Error: tmux not found. Install it with your package manager (e.g. apt install tmux / brew install tmux)" >&2
  exit 1
}

CLAUDE=$(command -v claude 2>/dev/null) || {
  echo "Error: claude not found. Install Claude Code from https://claude.ai/code" >&2
  exit 1
}

# --- Date flavor detection (TASK-319) ---
# GNU coreutils `date` supports --version and `-d <freeform>`; BSD/macOS `date`
# does not and needs `-j -f <fmt>` + `-v` adjustments. Detect once.
if date --version >/dev/null 2>&1; then
  DATE_FLAVOR="gnu"
else
  DATE_FLAVOR="bsd"
fi

# Parse a human date/time in timezone $1 to an epoch on stdout, or empty on
# failure. GNU takes a freeform string; BSD takes an strptime format + value.
#   _epoch_gnu <tz> <freeform>
#   _epoch_bsd <tz> <fmt> <value> [extra -j flags...]
_epoch_gnu() {
  local tz="$1" spec="$2"
  TZ="$tz" date -d "$spec" "+%s" 2>/dev/null || true
}
_epoch_bsd() {
  local tz="$1" fmt="$2" value="$3"; shift 3
  TZ="$tz" date -j "$@" -f "$fmt" "$value" "+%s" 2>/dev/null || true
}

# --- Pace calculation ---
# Returns pace as integer percent (100=on track, 130=30% over, 70=30% under)
# or empty string if calculation not possible.
# Args: $1=percent_used $2=reset_string $3=label_key
compute_pace() {
  local pct="$1"
  local reset="$2"
  local key="$3"
  local now_epoch reset_epoch window_seconds tz

  # Extra usage (pay-as-you-go) and session (unknown window start) don't apply.
  case "$reset" in *"spent"*) return ;; esac
  case "$key" in *"session"*) return ;; esac

  # Extract timezone from "(Region/City)".
  tz=$(echo "$reset" | grep -oE '\([A-Za-z_]+/[A-Za-z_]+\)' | tr -d '()')
  [[ -z "$tz" ]] && return

  now_epoch=$(date +%s)
  reset_epoch=""
  window_seconds=""

  if echo "$reset" | grep -qE 'Resets [A-Za-z]+ [0-9]+ at [0-9]+(:[0-9]+)?(am|pm)'; then
    # Weekly with date: "Resets Feb 20 at 1pm" / "Resets Feb 20 at 1:30pm"
    local date_part date_no_min
    date_part=$(echo "$reset" | grep -oE '[A-Za-z]+ [0-9]+ at [0-9]+(:[0-9]+)?(am|pm)')
    date_no_min=$(echo "$date_part" | sed 's/:[0-9][0-9]\(am\|pm\)/\1/')
    if [[ "$DATE_FLAVOR" == "gnu" ]]; then
      # "Feb 20 at 1pm" -> "Feb 20 1pm" for GNU's parser.
      reset_epoch=$(_epoch_gnu "$tz" "$(echo "$date_no_min" | sed 's/ at / /')")
    else
      reset_epoch=$(_epoch_bsd "$tz" "%b %d at %I%p" "$date_no_min")
    fi
    window_seconds=$((7 * 24 * 3600))

  elif echo "$reset" | grep -qE 'Resets [0-9]+(:[0-9]+)?(am|pm)'; then
    # Weekly reset today (date omitted): "Resets 9pm" / "Resets 4am"
    local time_part time_no_min today_in_tz
    time_part=$(echo "$reset" | grep -oE '[0-9]+(:[0-9]+)?(am|pm)')
    time_no_min=$(echo "$time_part" | sed 's/:[0-9][0-9]\(am\|pm\)/\1/')
    today_in_tz=$(TZ="$tz" date "+%b %d")
    if [[ "$DATE_FLAVOR" == "gnu" ]]; then
      reset_epoch=$(_epoch_gnu "$tz" "$today_in_tz $time_no_min")
    else
      reset_epoch=$(_epoch_bsd "$tz" "%b %d %I%p" "$today_in_tz $time_no_min")
    fi
    # If already past, it's tomorrow.
    [[ -n "$reset_epoch" && "$reset_epoch" -le "$now_epoch" ]] && reset_epoch=$((reset_epoch + 86400))
    window_seconds=$((7 * 24 * 3600))

  elif echo "$reset" | grep -qE 'Resets [A-Za-z]+ [0-9]+'; then
    # Monthly: "Resets Mar 1" (date only, resets at midnight).
    local month_day day_num mon_name
    month_day=$(echo "$reset" | grep -oE 'Resets [A-Za-z]+ [0-9]+' | sed 's/Resets //')
    if [[ "$DATE_FLAVOR" == "gnu" ]]; then
      reset_epoch=$(_epoch_gnu "$tz" "$month_day")
      # Roll to next year if the parsed date is already behind us.
      if [[ -n "$reset_epoch" && "$reset_epoch" -le "$now_epoch" ]]; then
        local next_year
        next_year=$(( $(TZ="$tz" date +%Y) + 1 ))
        reset_epoch=$(_epoch_gnu "$tz" "$month_day $next_year")
      fi
    else
      reset_epoch=$(_epoch_bsd "$tz" "%b %d" "$month_day")
      [[ -n "$reset_epoch" && "$reset_epoch" -le "$now_epoch" ]] && reset_epoch=$(_epoch_bsd "$tz" "%b %d" "$month_day" -v+1y)
    fi
    # Window = days in the month ending at this reset (last day = reset - 1 day).
    local days_in_month=""
    if [[ "$DATE_FLAVOR" == "gnu" && -n "$reset_epoch" ]]; then
      days_in_month=$(TZ="$tz" date -d "@$((reset_epoch - 86400))" "+%d" 2>/dev/null)
    elif [[ -n "$reset_epoch" ]]; then
      days_in_month=$(TZ="$tz" date -j -v-1d -f "%b %d" "$month_day" "+%d" 2>/dev/null)
    fi
    [[ -z "$days_in_month" ]] && days_in_month=30
    # Strip a possible leading zero so arithmetic doesn't treat it as octal.
    days_in_month=$((10#$days_in_month))
    window_seconds=$((days_in_month * 24 * 3600))
  else
    return
  fi

  [[ -z "$reset_epoch" || -z "$window_seconds" ]] && return

  local time_remaining=$(( reset_epoch - now_epoch ))
  [[ "$time_remaining" -le 0 ]] && return

  local elapsed=$(( window_seconds - time_remaining ))
  [[ "$elapsed" -le 0 ]] && return

  echo $(( pct * window_seconds / elapsed ))
}

# --- Launch Claude ---

SESSION="cclimits-$$"
CAPTURE=$(mktemp "${TMPDIR:-/tmp}/cclimits-XXXXXX.txt")
TIMEOUT=30

cleanup() {
  $TMUX kill-session -t "$SESSION" 2>/dev/null || true
  rm -f "$CAPTURE"
}
trap cleanup EXIT

wait_for() {
  local pattern="$1"
  local elapsed=0
  while ! $TMUX capture-pane -t "$SESSION" -p 2>/dev/null | grep -q "$pattern"; do
    sleep 0.3
    elapsed=$((elapsed + 1))
    if [[ $elapsed -gt $((TIMEOUT * 3)) ]]; then
      echo "Error: timed out waiting for Claude Code to show \"$pattern\"" >&2
      exit 1
    fi
  done
}

$TMUX new-session -d -s "$SESSION" -x 220 -y 50 "$CLAUDE" 2>/dev/null || {
  echo "Error: failed to start tmux session" >&2
  exit 1
}

wait_for "Claude Code"

# Send /usage — Escape dismisses autocomplete, Enter executes.
$TMUX send-keys -t "$SESSION" "/usage" ""
sleep 0.5
$TMUX send-keys -t "$SESSION" Escape
sleep 0.3
$TMUX send-keys -t "$SESSION" Enter

wait_for "Current session"

$TMUX capture-pane -t "$SESSION" -p > "$CAPTURE"

# --- Parse output ---

LINES=$(sed 's/\r//g' "$CAPTURE" \
  | grep -A 30 "Current session" \
  | grep -v "Esc to cancel" \
  | sed 's/[█▌▙▛▜▝▘─]//g' \
  | sed 's/^[[:space:]]*//' \
  | sed 's/[[:space:]]\{2,\}\([0-9]\)/\1/g' \
  | grep -v '^\s*$')

if [[ -z "$LINES" ]]; then
  echo "Error: failed to parse usage output" >&2
  exit 1
fi

# Parse lines into parallel arrays: labels, percents, resets, keys.
declare -a LABELS PERCENTS RESETS KEYS
i=0
while IFS= read -r label && IFS= read -r pct_line && IFS= read -r reset_line; do
  LABELS[$i]="$label"
  PERCENTS[$i]=$(echo "$pct_line" | grep -oE '[0-9]+' || echo "0")
  RESETS[$i]="$reset_line"
  key=$(echo "$label" | tr ' ' '_' | tr -cd 'a-zA-Z0-9_' | tr 'A-Z' 'a-z')
  KEYS[$i]="$key"
  i=$((i + 1))
done <<< "$LINES"

COUNT=$i

if $JSON; then
  printf "{"
  first=true
  for i in $(seq 0 $((COUNT - 1))); do
    key="${KEYS[$i]}"
    pct="${PERCENTS[$i]}"
    reset="${RESETS[$i]}"
    reset_escaped="${reset//\"/\\\"}"
    pace=$(compute_pace "$pct" "$reset" "$key")
    $first || printf ","
    if [[ -n "$pace" ]]; then
      printf '"%s":{"percent":%s,"pace":%s,"reset":"%s"}' "$key" "$pct" "$pace" "$reset_escaped"
    else
      printf '"%s":{"percent":%s,"reset":"%s"}' "$key" "$pct" "$reset_escaped"
    fi
    first=false
  done
  printf "}\n"
else
  for i in $(seq 0 $((COUNT - 1))); do
    echo "${LABELS[$i]}"
    echo "${PERCENTS[$i]}% used"
    pace=$(compute_pace "${PERCENTS[$i]}" "${RESETS[$i]}" "${KEYS[$i]}")
    [[ -n "$pace" ]] && echo "${pace}% pace"
    echo "${RESETS[$i]}"
    echo ""
  done
fi
