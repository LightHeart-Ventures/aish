#!/usr/bin/env bash
# Serialized, Claude-only build wrapper for coordinator / CI / multi-worktree
# builds — the two OOM mitigations from the build-stability review, in one
# place so every automated rebuild inherits them.
#
#   1. Don't compile mistralrs.  `--no-default-features` drops the heavy `local`
#      (mistralrs / candle / gemm) feature — the entire opt-level=3 phase and the
#      crate that peaks past 1.5 GB per rustc. Use this anywhere local in-process
#      inference isn't needed (every coordinator + CI build). Pass --features
#      local explicitly when you DO need it.
#
#   2. Serialize across worktrees.  Dozens of background-coordinator worktrees
#      can share one host; an unbounded set of concurrent `cargo build`s
#      overcommits RAM and trips the kernel OOM-killer (see .cargo/config.toml
#      for the companion per-build `jobs` cap). A single advisory file lock
#      (flock /tmp/aish-build.lock) bounds *cross-build* concurrency to 1: each
#      build still uses up to `jobs` cores internally, but only one worktree
#      compiles at a time, so N worktrees can't overcommit together.
#
# Usage:
#   scripts/build.sh                 # serialized, Claude-only debug build
#   scripts/build.sh --release       # serialized, Claude-only release build
#   scripts/build.sh --features local --release   # opt local inference back in
#
# Any arguments are forwarded verbatim to `cargo build`. Override the lock path
# with AISH_BUILD_LOCK and the cargo binary with CARGO.
set -euo pipefail

CARGO="${CARGO:-cargo}"
LOCK="${AISH_BUILD_LOCK:-/tmp/aish-build.lock}"

# Default to a Claude-only build, but let the caller opt local inference back in:
# if they pass any --features / --no-default-features / --all-features flag we
# leave feature selection entirely to them.
feature_args=(--no-default-features)
for arg in "$@"; do
  case "$arg" in
    --features|--features=*|--no-default-features|--all-features)
      feature_args=()
      break
      ;;
  esac
done

# flock is util-linux (Linux only). On macOS / hosts without it, fall back to an
# unserialized build rather than failing — the per-build jobs cap still applies.
if command -v flock >/dev/null 2>&1; then
  exec flock "$LOCK" "$CARGO" build "${feature_args[@]}" "$@"
else
  echo "scripts/build.sh: flock not found — building without the cross-worktree lock" >&2
  exec "$CARGO" build "${feature_args[@]}" "$@"
fi
