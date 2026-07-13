#!/usr/bin/env bash
# ci-triage.sh — pull the ACTUAL failure from an aish CI or Release run instead
# of guessing from source (SKILL.md triage step 2: "Read the ACTUAL failure, not
# the source"). Shows the recent run graph, then dumps the failed-step logs of
# the target run (default: the most recent failed CI or Release run).
#
# Read-only. Wraps `gh run list/view --log-failed`.
#
# Usage:
#   ci-triage.sh                 # newest FAILED run across CI + Release
#   ci-triage.sh 28434180793     # a specific run id
#   ci-triage.sh --release       # newest FAILED Release-workflow run
#   ci-triage.sh --ci            # newest FAILED CI-workflow run
#   REPO=owner/repo ci-triage.sh
#
# Env:
#   REPO   default LightHeart-Ventures/aish
#   TAIL   lines of failed-log to show (default 120; 0 = full)

set -uo pipefail

command -v gh >/dev/null || { echo "ci-triage: gh not found on PATH" >&2; exit 2; }

REPO="${REPO:-LightHeart-Ventures/aish}"
TAIL="${TAIL:-120}"

WF_FILTER=""
RUN_ID=""
case "${1:-}" in
  --release) WF_FILTER="release" ;;
  --ci)      WF_FILTER="ci" ;;
  "" )       : ;;
  * )        RUN_ID="$1" ;;
esac

printf '\033[1maish ci-triage\033[0m — %s\n\n' "$REPO"

# ---- recent run graph -------------------------------------------------------
printf '\033[1mRecent runs\033[0m\n'
gh run list --repo "$REPO" --limit 12 \
  --json databaseId,workflowName,headBranch,event,status,conclusion,createdAt \
  -q '.[] | "  \(.databaseId)  \(.conclusion // .status | ascii_upcase)  \(.workflowName)  [\(.headBranch)]  \(.createdAt[0:16])"' \
  2>/dev/null || { echo "ci-triage: gh run list failed (auth? repo?)" >&2; exit 2; }

# ---- resolve the target run -------------------------------------------------
if [ -z "$RUN_ID" ]; then
  # newest failed run, optionally filtered by workflow name substring
  RUN_ID="$(gh run list --repo "$REPO" --limit 40 \
    --json databaseId,workflowName,conclusion \
    -q "[.[] | select(.conclusion==\"failure\")
             | select(\"$WF_FILTER\"==\"\" or (.workflowName|ascii_downcase|contains(\"$WF_FILTER\")))]
        | .[0].databaseId // empty" 2>/dev/null)"
  if [ -z "$RUN_ID" ]; then
    printf '\n\033[32mNo failed runs%s in the recent window.\033[0m\n' "$([ -n "$WF_FILTER" ] && echo " for '$WF_FILTER'")"
    exit 0
  fi
fi

# ---- job graph for the target run ------------------------------------------
printf '\n\033[1mRun %s — job graph\033[0m\n' "$RUN_ID"
gh run view "$RUN_ID" --repo "$REPO" \
  --json displayTitle,workflowName,headBranch,event,status,conclusion,jobs \
  -q '"  \(.workflowName): \(.displayTitle)  (\(.conclusion // .status))",
      (.jobs[] | "    [\(.conclusion // .status | ascii_upcase)] \(.name)"
        + ([.steps[] | select((.conclusion // "")=="failure") | "  ✗ "+.name] | join("")))' \
  2>/dev/null || echo "  (could not fetch job graph for $RUN_ID)"

# ---- failed-step logs -------------------------------------------------------
printf '\n\033[1mFailed-step logs\033[0m'
if [ "$TAIL" = "0" ]; then
  printf ' (full)\n'
  gh run view "$RUN_ID" --repo "$REPO" --log-failed 2>/dev/null || echo "  (no --log-failed output; try: gh run view $RUN_ID --log)"
else
  printf ' (last %s lines — set TAIL=0 for full)\n' "$TAIL"
  gh run view "$RUN_ID" --repo "$REPO" --log-failed 2>/dev/null | tail -n "$TAIL" \
    || echo "  (no --log-failed output; try: gh run view $RUN_ID --log)"
fi

printf '\n\033[2mFull logs: gh run view %s --repo %s --log   ·   watch: gh run watch %s --repo %s\033[0m\n' \
  "$RUN_ID" "$REPO" "$RUN_ID" "$REPO"

# ---- section pointer --------------------------------------------------------
printf '\033[2mNext: version-guard fail → §2/§4 · OOM/killed → §3 · immutable/asset 422 → §1 · oracle/test → §4.\033[0m\n'
