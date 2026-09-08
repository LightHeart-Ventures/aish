#!/usr/bin/env bash
# registry-stars.sh — best-effort star ratings for skills.
#
# Reads the OFFLINE registry index (~/.aish/registry/skills.json — the same
# file the aish engine ships and reads via skill_provider::local_index_catalog,
# née index.json before the JSONL split in commit bf29105). The bundled index
# does carry a `"stars"` field for every entry today. Presentation only: never
# let a star count override a clearly-better keyword match. The network is
# never touched.
#
# Usage: registry-stars.sh <skill-name> [<skill-name> …]
# Output: JSON  {"<name>": {"stars": N}, …}

set -eu

index="${HOME}/.aish/registry/skills.json"

stars_for() {
  local want="$1"
  [[ -f "$index" ]] || { printf '0'; return; }
  if command -v jq >/dev/null 2>&1; then
    jq -r --arg n "$want" \
      '(.results[]? | select(.name==$n) | .stars) // 0' "$index" 2>/dev/null \
      | head -1 | grep -E '^[0-9]+$' || printf '0'
  else
    printf '0'
  fi
}

printf '{'
first=true
for name in "$@"; do
  $first || printf ','
  first=false
  printf '"%s":{"stars":%s}' "$name" "$(stars_for "$name")"
done
printf '}\n'
