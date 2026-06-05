#!/usr/bin/env bash
# Launch claude with the "aish" Atum credentials profile exported.
set -euo pipefail

CREDENTIALS_FILE="${ATUM_CREDENTIALS_FILE:-$HOME/.atum/credentials}"
PROFILE="aish"

if [[ ! -f "$CREDENTIALS_FILE" ]]; then
  echo "aish.sh: credentials file not found: $CREDENTIALS_FILE" >&2
  exit 1
fi

# Extract KEY=VALUE lines from the [aish] section and export them.
in_section=0
found=0
while IFS= read -r line; do
  case "$line" in
    "[$PROFILE]") in_section=1; found=1; continue ;;
    \[*\]) in_section=0; continue ;;
  esac
  if [[ $in_section -eq 1 && "$line" =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; then
    export "$line"
  fi
done < "$CREDENTIALS_FILE"

if [[ $found -eq 0 ]]; then
  echo "aish.sh: profile [$PROFILE] not found in $CREDENTIALS_FILE" >&2
  exit 1
fi

exec claude --worktree --dangerously-skip-permissions "$@"
