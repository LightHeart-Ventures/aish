#!/usr/bin/env bash
# remove-worktree-on-complete.sh — SPR-064 / TASK-327 (FR-328)
#
# Board-driven worktree auto-delete. Invoked when a card moves to col_completed
# (wire it as an Atum board automation action, a webhook handler, or a local
# post-merge hook — see DEVELOPMENT.md). Force-removes the coordinator worktree
# tied to a finished task and prunes its merged branch, tying worktree lifecycle
# to task lifecycle so trees are cleaned up the moment their task is done.
#
# NO-OP SAFE: if the worktree is already gone, it logs and exits 0.
#
# Usage:
#   scripts/remove-worktree-on-complete.sh <worktree-id | branch | path>
# Examples:
#   scripts/remove-worktree-on-complete.sh w_iUWGneQL
#   scripts/remove-worktree-on-complete.sh aish/w_iUWGneQL
#   scripts/remove-worktree-on-complete.sh feat/task-999-foo
set -euo pipefail

id="${1:-}"
if [ -z "$id" ]; then
  echo "usage: $0 <worktree-id | branch | path>" >&2
  exit 2
fi

root="$(git rev-parse --show-toplevel)"
log="$root/.worktree-cleanup.log"
logline() { printf '%s\n' "$1"; printf '[%s] %s\n' "$(date -u +%FT%TZ)" "$1" >> "$log" 2>/dev/null || true; }

trunk="origin/main"
git rev-parse --verify --quiet "$trunk" >/dev/null 2>&1 || trunk="main"

# Locate the matching worktree by path suffix or branch name.
match_path=""; match_branch=""; match_head=""
path=""; branch=""; head=""
scan() {
  [ -z "$path" ] && return 0
  local name; name="$(basename "$path")"
  if [ "$path" = "$id" ] || [ "$name" = "$id" ] || [ "$name" = "${id##*/}" ] \
     || [ -n "$branch" ] && { [ "$branch" = "$id" ] || [ "$branch" = "aish/$id" ]; }; then
    match_path="$path"; match_branch="$branch"; match_head="$head"
  fi
}
while IFS= read -r line; do
  case "$line" in
    worktree\ *) scan; path="${line#worktree }"; branch=""; head="" ;;
    HEAD\ *)     head="${line#HEAD }" ;;
    branch\ *)   branch="$(echo "${line#branch }" | sed 's#^refs/heads/##')" ;;
  esac
done < <(git worktree list --porcelain)
scan

if [ -z "$match_path" ]; then
  logline "no-op: no worktree matches '$id' (already removed?)"
  git worktree prune 2>/dev/null || true
  exit 0
fi

if git worktree remove --force "$match_path" 2>>"$log"; then
  logline "removed worktree $(basename "$match_path") (task complete) branch=${match_branch:-DETACHED}"
else
  logline "no-op: git worktree remove failed or already gone for $match_path"
  git worktree prune 2>/dev/null || true
  exit 0
fi

# Prune the branch only when it is merged into trunk.
if [ -n "$match_branch" ] && [ -n "$match_head" ] \
   && git merge-base --is-ancestor "$match_head" "$trunk" 2>/dev/null; then
  if git branch -D "$match_branch" >>"$log" 2>&1; then
    logline "  pruned merged branch $match_branch"
  fi
else
  [ -n "$match_branch" ] && logline "  kept branch $match_branch (not merged to $trunk)"
fi

git worktree prune 2>/dev/null || true
exit 0
