#!/usr/bin/env bash
# coordinator-triage.sh — diagnose a background coordinator run from its journal
# (SKILL.md §6). Every run writes .atum/run-<id>.jsonl: one record per tool call
# (input+output) plus a per-round record with status:"synthesis". A run that
# repeats the same synthesis round after round is LOOPING — visible here even
# when the tool log alone hides it.
#
# Read-only. Reports: round count vs the default cap (48), tool-call tally,
# repeated-synthesis detection (the loop signal), whether a blocker was declared
# (a clearly-stated blocker is a SUCCESS, §6), and the last synthesis text.
#
# Usage:
#   coordinator-triage.sh                       # newest .atum/run-*.jsonl under CWD
#   coordinator-triage.sh w_SlcE27iD            # by run id (finds .atum/run-w_SlcE27iD.jsonl)
#   coordinator-triage.sh path/to/run-xxx.jsonl # explicit journal path
#   ATUM_DIR=/repo/.atum coordinator-triage.sh <id>
#
# Env:
#   ATUM_DIR       journal dir (default: ./.atum)
#   MAX_ROUNDS     round cap to compare against (default: 48 = AISH_COORDINATOR_MAX_ROUNDS)

set -uo pipefail

command -v jq >/dev/null || { echo "coordinator-triage: jq not found on PATH" >&2; exit 2; }

ATUM_DIR="${ATUM_DIR:-.atum}"
MAX_ROUNDS="${MAX_ROUNDS:-48}"

# ---- resolve the journal file ----------------------------------------------
arg="${1:-}"
JOURNAL=""
if [ -n "$arg" ] && [ -f "$arg" ]; then
  JOURNAL="$arg"
elif [ -n "$arg" ]; then
  # treat as a run id (with or without .jsonl / run- prefix)
  id="${arg%.jsonl}"; id="${id#run-}"
  for cand in "$ATUM_DIR/run-$id.jsonl" "$ATUM_DIR/$id.jsonl" "$ATUM_DIR/run-$arg" ; do
    [ -f "$cand" ] && { JOURNAL="$cand"; break; }
  done
  [ -z "$JOURNAL" ] && { echo "coordinator-triage: no journal for run id '$arg' under $ATUM_DIR" >&2; exit 2; }
else
  # newest journal in ATUM_DIR
  JOURNAL="$(ls -1t "$ATUM_DIR"/run-*.jsonl 2>/dev/null | head -1)"
  [ -z "$JOURNAL" ] && { echo "coordinator-triage: no .atum/run-*.jsonl found under $ATUM_DIR (run from the repo, or pass a path/id)" >&2; exit 2; }
fi

printf '\033[1maish coordinator-triage\033[0m — %s\n' "$JOURNAL"

# Guard against a non-JSONL file.
if ! head -1 "$JOURNAL" | jq -e . >/dev/null 2>&1; then
  echo "coordinator-triage: $JOURNAL is not valid JSONL (first line failed jq parse)" >&2
  exit 2
fi

TOTAL="$(wc -l < "$JOURNAL" | tr -d ' ')"

# Records are heterogeneous across versions; probe defensively with `// empty`.
# A synthesis record carries status=="synthesis"; everything else we treat as a
# tool/other record. Synthesis text is looked up under a few likely keys.
SYNTH_COUNT="$(jq -rs '[.[] | select((.status? // "") == "synthesis")] | length' "$JOURNAL" 2>/dev/null || echo 0)"
TOOL_COUNT="$(jq -rs '[.[] | select((.status? // "") != "synthesis") | select((.tool? // .tool_name? // .name? // "") != "")] | length' "$JOURNAL" 2>/dev/null || echo 0)"

printf '\n\033[1mVolume\033[0m\n'
printf '  records: %s   synthesis rounds: %s   tool calls: %s\n' "$TOTAL" "$SYNTH_COUNT" "$TOOL_COUNT"

# ---- round cap check (§6) ---------------------------------------------------
printf '\n\033[1mRound budget (§6)\033[0m\n'
if [ "${SYNTH_COUNT:-0}" -ge "$MAX_ROUNDS" ]; then
  printf '  \033[31m%s synthesis rounds ≥ cap %s\033[0m — budget-exhausted. Read the loop signal below BEFORE raising AISH_COORDINATOR_MAX_ROUNDS (bumping the cap brute-forces a loop; §6).\n' "$SYNTH_COUNT" "$MAX_ROUNDS"
elif [ "${SYNTH_COUNT:-0}" -ge $((MAX_ROUNDS * 3 / 4)) ]; then
  printf '  \033[33m%s/%s rounds\033[0m — approaching the cap.\n' "$SYNTH_COUNT" "$MAX_ROUNDS"
else
  printf '  %s/%s rounds — within budget.\n' "$SYNTH_COUNT" "$MAX_ROUNDS"
fi

# ---- repeated-synthesis loop detection --------------------------------------
printf '\n\033[1mLoop detection (repeated synthesis)\033[0m\n'
# Collect synthesis texts (try common field names), hash-normalise whitespace,
# and report the most frequent. >1 identical synthesis is the loop tell.
DUP="$(jq -rs '
  [ .[]
    | select((.status? // "") == "synthesis")
    | (.synthesis? // .text? // .message? // .summary? // "")
    | gsub("\\s+"; " ") | gsub("^ | $"; "")
    | select(. != "")
  ]
  | group_by(.) | map({n: length, t: .[0]}) | sort_by(-.n) | .[0] // empty
' "$JOURNAL" 2>/dev/null)"

if [ -z "$DUP" ]; then
  printf '  no synthesis text field found to compare (older journal schema?) — inspect manually: jq -c "select(.status==\\"synthesis\\")" %s\n' "$JOURNAL"
else
  N="$(printf '%s' "$DUP" | jq -r '.n')"
  T="$(printf '%s' "$DUP" | jq -r '.t' | cut -c1-160)"
  if [ "${N:-0}" -ge 3 ]; then
    printf '  \033[31mLOOP: the same synthesis repeated %sx\033[0m — the model is retrying a failing approach. Fix the underlying blocker or steer the run; do not just raise the cap (§6).\n' "$N"
    printf '  repeated text: \033[2m%s…\033[0m\n' "$T"
  elif [ "${N:-0}" -eq 2 ]; then
    printf '  \033[33mpossible loop: one synthesis appeared 2x\033[0m — watch for a third. text: \033[2m%s…\033[0m\n' "$T"
  else
    printf '  no repeated synthesis (each round distinct) — not a classic loop.\n'
  fi
fi

# ---- blocker declared? (a stated blocker is a SUCCESS, §6) -------------------
printf '\n\033[1mBlocker (§6 — a clearly-stated blocker is a SUCCESS)\033[0m\n'
BLOCKER="$(jq -rs '
  [ .[] | (.synthesis? // .text? // .message? // .summary? // "")
    | select(test("(?i)\\b(blocked|i am blocked|cannot proceed|blocker)\\b")) ] | last // empty
' "$JOURNAL" 2>/dev/null)"
if [ -n "$BLOCKER" ]; then
  printf '  \033[32mblocker declared\033[0m — treat as terminal/expected, not a hang:\n  \033[2m%s\033[0m\n' "$(printf '%s' "$BLOCKER" | head -c 300)"
else
  printf '  none declared. If the run is still going and looping above, that is the problem to fix (not the cap).\n'
fi

# ---- last synthesis for context --------------------------------------------
printf '\n\033[1mLast synthesis\033[0m\n'
LAST="$(jq -rs '[.[] | select((.status? // "") == "synthesis") | (.synthesis? // .text? // .message? // .summary? // "")] | last // empty' "$JOURNAL" 2>/dev/null)"
if [ -n "$LAST" ]; then
  printf '  \033[2m%s\033[0m\n' "$(printf '%s' "$LAST" | head -c 500)"
else
  printf '  (no synthesis records)\n'
fi

printf '\n\033[2mFull journal: less %s   ·   tail live: tail -f %s\033[0m\n' "$JOURNAL" "$JOURNAL"
