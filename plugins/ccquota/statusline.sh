#!/usr/bin/env bash
#
# statusline.sh — first-class ccquota SecondStatusLine segment (TASK-318).
#
# This is the Phase 2b (`provides.statusline`) entrypoint. aish CORE owns the
# refresh cadence, the in-memory cache, the per-run timeout, and staleness now —
# it runs THIS script on its own schedule (see `every` in plugin.json), off the
# agent turn loop, and folds our first stdout line onto the status line. So this
# script does exactly one thing: print one colored badge line and exit.
#
# Contrast with the old Phase 1 design (refresh.sh + a TurnEnd hook): there the
# plugin owned the throttle stamp, the single-flight lock, the detached capture,
# and the `~/.aish/state/statusline/ccquota.txt` cache path. All of that is gone
# — core handles it. No throttle, no lock, no file writes, no detaching.
#
# Always exits 0 with either a badge on stdout or nothing (core keeps the prior
# segment until it ages out). A status badge must never be an error.

set -u

PLUGIN_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CCLIMITS="$PLUGIN_DIR/cclimits.sh"

# Every dependency degrades to "no badge", never a failure.
[[ -x "$CCLIMITS" ]] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0

# cclimits.sh drives `claude` through a headless tmux session to read /usage;
# it can take several seconds. That's fine — core runs us with a timeout on a
# background task, not on the keystroke path.
json="$("$CCLIMITS" --json 2>/dev/null)" || exit 0
[[ -z "$json" ]] && exit 0

# badge.py prints ONE ready-to-render, plugin-colored line (or nothing on
# unusable input). Core reads that first non-empty line as the segment.
printf '%s' "$json" | python3 "$PLUGIN_DIR/badge.py" 2>/dev/null || exit 0

exit 0
