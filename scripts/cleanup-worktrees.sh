#!/usr/bin/env bash
# cleanup-worktrees.sh — SPR-064 / TASK-328 (FR-328)
#
# Nightly TTL + merged-to-main worktree reaper. Enumerates git worktrees and
# removes any whose branch is merged into origin/main OR that have been idle
# longer than the retention TTL, then prunes their merged branches. Reclaims the
# leaked disk from zombie coordinator trees and holds steady-state idle trees low.
#
# SAFE BY DEFAULT: dry-run unless --apply is passed. Never touches the primary
# worktree, the current worktree, or a branch that is NOT merged to trunk.
#
# Usage:
#   scripts/cleanup-worktrees.sh            # dry-run (prints what it WOULD do)
#   scripts/cleanup-worktrees.sh --apply    # actually remove + prune
#   scripts/cleanup-worktrees.sh --apply --ttl 45
set -euo pipefail

apply=0
ttl_override=""
while [ $# -gt 0 ]; do
  case "$1" in
    --apply) apply=1 ;;
    --dry-run) apply=0 ;;
    --ttl) shift; ttl_override="$1" ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
  shift
done

root="$(git rev-parse --show-toplevel)"
current="$(cd "$root" && pwd -P)"
log="$root/.worktree-cleanup.log"

ttl_days=30
if command -v python3 >/dev/null 2>&1 && [ -f "$root/.repospec.json" ]; then
  ttl_days="$(python3 -c "import json;print(json.load(open('$root/.repospec.json')).get('worktreeRetention',{}).get('ttlIdleDays',30))" 2>/dev/null || echo 30)"
fi
[ -n "$ttl_override" ] && ttl_days="$ttl_override"

trunk="origin/main"
git rev-parse --verify --quiet "$trunk" >/dev/null 2>&1 || trunk="main"

# Primary worktree = first `worktree ` line of the porcelain output.
primary="$(git worktree list --porcelain | awk '/^worktree /{print $2; exit}')"

now_epoch="$(date +%s)"
removed=0; pruned=0; scanned=0
mode="DRY-RUN"; [ "$apply" = "1" ] && mode="APPLY"

logline() { printf '%s\n' "$1"; [ "$apply" = "1" ] && printf '[%s] %s\n' "$(date -u +%FT%TZ)" "$1" >> "$log" || true; }

logline "=== cleanup-worktrees $mode (ttl=${ttl_days}d, trunk=$trunk) ==="

path=""; branch=""; head=""; detached=0
consider() {
  [ -z "$path" ] && return 0
  scanned=$((scanned+1))
  local rp; rp="$(cd "$path" 2>/dev/null && pwd -P || echo "$path")"
  # Never remove primary or current worktree.
  if [ "$rp" = "$primary" ] || [ "$rp" = "$current" ]; then return 0; fi

  local merged=0 idle=0 age_days commit_age
  commit_age="$(git -C "$path" log -1 --format=%ct 2>/dev/null || echo "$now_epoch")"
  age_days=$(( (now_epoch - commit_age) / 86400 ))
  if [ -n "$branch" ] && [ -n "$head" ] && git merge-base --is-ancestor "$head" "$trunk" 2>/dev/null; then
    merged=1
  fi
  [ "$age_days" -gt "$ttl_days" ] && idle=1

  if [ "$merged" = "0" ] && [ "$idle" = "0" ]; then
    return 0   # keep: unmerged and within TTL
  fi

  local reason="merged"; [ "$merged" = "0" ] && reason="idle>${ttl_days}d(${age_days}d)"
  local name; name="$(basename "$path")"
  if [ "$apply" = "1" ]; then
    if git worktree remove --force "$path" 2>>"$log"; then
      removed=$((removed+1)); logline "removed worktree $name ($reason) branch=${branch:-DETACHED}"
      # Only delete the branch if it is merged to trunk.
      if [ "$merged" = "1" ] && [ -n "$branch" ]; then
        if git branch -D "$branch" >>"$log" 2>&1; then pruned=$((pruned+1)); logline "  pruned merged branch $branch"; fi
      fi
    else
      logline "FAILED to remove $name (see log)"
    fi
  else
    logline "would remove $name ($reason) branch=${branch:-DETACHED} age=${age_days}d"
  fi
}

while IFS= read -r line; do
  case "$line" in
    worktree\ *) consider; path="${line#worktree }"; branch=""; head=""; detached=0 ;;
    HEAD\ *)     head="${line#HEAD }" ;;
    branch\ *)   branch="$(echo "${line#branch }" | sed 's#^refs/heads/##')" ;;
    detached)    detached=1 ;;
  esac
done < <(git worktree list --porcelain)
consider

if [ "$apply" = "1" ]; then
  git worktree prune >>"$log" 2>&1 || true
fi

logline "=== $mode complete: scanned=$scanned removed=$removed branches_pruned=$pruned ==="
[ "$apply" = "0" ] && echo "(dry-run — re-run with --apply to perform the above removals)"
exit 0
