#!/usr/bin/env bash
# audit-worktrees.sh — SPR-064 / TASK-329 (FR-328)
#
# Read-only observability pass over all git worktrees. Classifies each worktree
# ACTIVE vs STALE and emits a CSV so worktree debt is visible instead of silent.
# STALE = its branch is merged into origin/main (ancestor of the trunk tip), OR
# the worktree has been idle longer than the retention TTL, OR it is detached.
#
# Usage:
#   scripts/audit-worktrees.sh [output.csv]
# Default output: <repo-root>/.worktree-audit.csv
# Exit code is always 0 (audit never fails the caller); STALE count is printed.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
out="${1:-$root/.worktree-audit.csv}"

# Default TTL. .repospec.json's worktreeRetention integration was removed
# from this repo (see DEVELOPMENT.md#retention-policy); the block below is a
# no-op today unless a caller re-introduces that file, and is kept only so a
# future repo-level override still works without a code change.
ttl_days=30
if command -v python3 >/dev/null 2>&1 && [ -f "$root/.repospec.json" ]; then
  ttl_days="$(python3 -c "import json;print(json.load(open('$root/.repospec.json')).get('worktreeRetention',{}).get('ttlIdleDays',30))" 2>/dev/null || echo 30)"
fi

# Resolve the trunk ref we compare merges against.
trunk="origin/main"
git rev-parse --verify --quiet "$trunk" >/dev/null 2>&1 || trunk="main"

now_epoch="$(date +%s)"
active=0
stale=0

echo "STATE,name,branch,commit,age_days,path" > "$out"

path=""; branch=""; head=""; detached=0
emit_row() {
  [ -z "$path" ] && return 0
  local name age_days state commit_age
  name="$(basename "$path")"
  commit_age="$(git -C "$path" log -1 --format=%ct 2>/dev/null || echo "$now_epoch")"
  age_days=$(( (now_epoch - commit_age) / 86400 ))
  local br="${branch:-DETACHED}"
  state="ACTIVE"
  if [ "$detached" = "1" ]; then
    state="STALE"
  elif [ -n "$branch" ] && git merge-base --is-ancestor "$head" "$trunk" 2>/dev/null; then
    state="STALE"   # merged into trunk
  elif [ "$age_days" -gt "$ttl_days" ]; then
    state="STALE"   # idle beyond TTL
  fi
  echo "$state,$name,$br,${head:0:9},$age_days,$path" >> "$out"
  if [ "$state" = "STALE" ]; then stale=$((stale+1)); else active=$((active+1)); fi
}

while IFS= read -r line; do
  case "$line" in
    worktree\ *) emit_row; path="${line#worktree }"; branch=""; head=""; detached=0 ;;
    HEAD\ *)     head="${line#HEAD }" ;;
    branch\ *)   branch="$(echo "${line#branch }" | sed 's#^refs/heads/##')" ;;
    detached)    detached=1 ;;
    "")          : ;;
  esac
done < <(git worktree list --porcelain)
emit_row

total=$((active + stale))
echo "----------------------------------------"
echo "Worktree audit  (TTL=${ttl_days}d, trunk=${trunk})"
echo "  total : $total"
echo "  active: $active"
echo "  stale : $stale"
if command -v du >/dev/null 2>&1; then
  wroot="$(dirname "$root")"
  echo "  disk  : $(du -sh "$HOME/.aish/worktrees" 2>/dev/null | cut -f1 || echo n/a) under ~/.aish/worktrees"
fi
echo "  csv   : $out"
echo "----------------------------------------"
exit 0
