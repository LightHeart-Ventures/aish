#!/usr/bin/env bash
# check-branch-freshness.sh — SPR-064 / TASK-326
#
# CI fresh-branch validation gate. Rejects worktrees/pushes checked out on a
# STALE `release/*` branch — i.e. a release branch whose version is not the
# latest `release/*` tag. Prevents the "Feature Branch Duplicate" and
# stale-release collision classes at the source: a worker that spawns on an old
# release/* branch (a common worktree-sprawl trigger) fails CI immediately with
# a clear message naming the latest release.
#
# Fresh feat/*, fix/*, docs/*, aish/w_* branches always pass unchanged.
#
# Usage:
#   scripts/check-branch-freshness.sh [branch-name]
# Branch resolution order: $1 -> $GITHUB_HEAD_REF -> $GITHUB_REF_NAME -> HEAD.
# Escape hatch: ALLOW_STALE_RELEASE=1 forces a pass (e.g. cutting the newest
# release before its tag exists).
set -euo pipefail

branch="${1:-${GITHUB_HEAD_REF:-${GITHUB_REF_NAME:-$(git rev-parse --abbrev-ref HEAD)}}}"

case "$branch" in
  release/*) ;;
  *)
    echo "OK: '$branch' is not a release/* branch — freshness gate not applicable."
    exit 0
    ;;
esac

if [ "${ALLOW_STALE_RELEASE:-0}" = "1" ]; then
  echo "OK: ALLOW_STALE_RELEASE=1 set — skipping freshness gate for '$branch'."
  exit 0
fi

# Latest release tag by version. aish tags releases as vX.Y.Z (not release/*),
# so match the v* tag set; fall back to release/* tags if that convention changes.
latest_tag="$(git tag --list 'v*' --sort=-version:refname | head -1 || true)"
[ -z "$latest_tag" ] && latest_tag="$(git tag --list 'release/*' --sort=-version:refname | head -1 || true)"
if [ -z "$latest_tag" ]; then
  echo "OK: no release tags exist yet — nothing to compare '$branch' against."
  exit 0
fi

bver="${branch#release/}"        # release/v0.30.0 -> v0.30.0
lver="${latest_tag#release/}"    # v0.34.0 (or release/v0.34.0 -> v0.34.0)

if [ "$bver" = "$lver" ]; then
  echo "OK: '$branch' matches the latest release tag '$latest_tag'."
  exit 0
fi

# Is the branch's version the highest of the two? If so, allow (it is ahead of
# the latest tag — a legitimate in-progress newer release).
highest="$(printf '%s\n%s\n' "${bver#v}" "${lver#v}" | sort -V | tail -1)"
if [ "${bver#v}" = "$highest" ]; then
  echo "OK: '$branch' is newer than the latest release tag '$latest_tag' — allowed."
  exit 0
fi

cat >&2 <<EOF
STALE RELEASE BRANCH: '$branch' is behind the latest release tag '$latest_tag'.
This branch is a stale/duplicate release worktree and would collide on merge.
Rebase onto (or branch from) the latest release, or use a feat/*, fix/*, docs/*
branch. Latest release tag: $latest_tag
(Override with ALLOW_STALE_RELEASE=1 only if you are intentionally cutting it.)
EOF
exit 1
