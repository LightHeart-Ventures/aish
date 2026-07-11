#!/usr/bin/env bash
# signoz-observability :: statusline segment
# Prints the current exception summary (or a clean marker) for the
# SecondStatusLine. Kept trivial and fast (<1.5s budget).
set -euo pipefail
PLUGIN_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$PLUGIN_DIR/state/signoz/exceptions.txt"
if [ -s "$OUT" ]; then
  # already prefixed with ⚠ by the poller; keep it short for the status line
  head -c 160 "$OUT" | tr -d '\n'
else
  printf '📡 signoz: clean'
fi
