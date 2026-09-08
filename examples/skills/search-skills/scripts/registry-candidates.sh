#!/usr/bin/env bash
# registry-candidates.sh — rank INSTALLABLE registry skills for a task.
#
# Used by the no-local-match path: when no installed skill fits, recommend an
# installable one. Reads the SAME offline registry index the aish engine's
# recommend_install reads (~/.aish/registry/skills.json, née index.json before
# the JSONL split in commit bf29105), scores each entry with the engine's
# name-weighted relevance rule, and prints the best matches that are NOT
# already installed. Network is never touched.
#
# Usage: registry-candidates.sh "<task description>"
# Output: ranked JSON lines  {"reference":…,"name":…,"score":N,"description":…}

set -eu

task="${1:-}"
index="${HOME}/.aish/registry/skills.json"
skills_dir="${HOME}/.aish/skills"

[[ -f "$index" ]] || { echo '[]'; exit 0; }
command -v jq >/dev/null 2>&1 || { echo '[]'; exit 0; }   # need jq to parse

# task keywords (same tokenization as discover-skills.sh)
stop='^(the|a|an|to|for|with|on|at|in|of|or|and|is|it|this|that|my|me|i|help)$'
mapfile -t kw < <(printf '%s' "$task" \
  | tr '[:upper:]' '[:lower:]' \
  | tr -cs 'a-z0-9' '\n' \
  | awk 'length>=3' \
  | grep -Ev "$stop" \
  | sort -u)

# names already installed → skip them (the local nudge already covers those).
installed=""
if [[ -d "$skills_dir" ]]; then
  installed=$(find "$skills_dir" -name SKILL.md -type f 2>/dev/null \
    | while IFS= read -r f; do basename "$(dirname "$f")"; done | tr '\n' ' ')
fi

score_one() {
  local name="$1" desc="$2" t score=0
  name=$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]')
  desc=$(printf '%s' "$desc" | tr '[:upper:]' '[:lower:]')
  for t in "${kw[@]:-}"; do
    [[ -z "$t" ]] && continue
    if printf '%s' "$name" | tr -cs 'a-z0-9' '\n' | grep -qxF "$t"; then
      score=$((score + 3))
    elif printf '%s' "$desc" | tr -cs 'a-z0-9' '\n' | grep -qxF "$t"; then
      score=$((score + 1))
    fi
  done
  printf '%s' "$score"
}

tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT

# Stream each registry entry as: name \t reference \t description
jq -r '.results[]? | [.name, .reference, .description] | @tsv' "$index" \
  | while IFS=$'\t' read -r name ref desc; do
      case " $installed " in *" $name "*) continue;; esac   # skip installed
      s=$(score_one "$name" "$desc")
      [[ "$s" -ge 3 ]] || continue                          # engine MIN_SCORE
      printf '%s\t%s\t%s\t%s\n' "$s" "$ref" "$name" "$desc" >>"$tmp"
    done

sort -t$'\t' -k1,1nr "$tmp" | while IFS=$'\t' read -r s ref name desc; do
  esc_desc=${desc//\"/\\\"}
  printf '{"reference":"%s","name":"%s","score":%s,"description":"%s"}\n' \
    "$ref" "$name" "$s" "$esc_desc"
done
