#!/usr/bin/env bash
# discover-skills.sh — rank installed skills by relevance to a task.
#
# Scans ~/.aish/skills/*/SKILL.md, extracts name + description, and scores each
# skill with the SAME name-weighted relevance rule the aish engine uses
# (src/skill_match.rs::relevance): for each DISTINCT task keyword, a NAME-token
# match scores 3, else a DESCRIPTION-token match scores 1, else 0. This keeps
# the ranked table consistent with the automatic `[aish skill-awareness]` nudge
# instead of inventing a competing score.
#
# Usage: discover-skills.sh "<task description>"
# Output: ranked JSON lines  {"name":…,"score":N,"description":…,"installed":true}

set -eu

task="${1:-}"
skills_dir="${HOME}/.aish/skills"

[[ -d "$skills_dir" ]] || { echo '[]'; exit 0; }

# --- tokenize the task: lowercase, split on non-alphanumerics, drop stop/short.
stop='^(the|a|an|to|for|with|on|at|in|of|or|and|is|it|this|that|my|me|i|help)$'
mapfile -t kw < <(printf '%s' "$task" \
  | tr '[:upper:]' '[:lower:]' \
  | tr -cs 'a-z0-9' '\n' \
  | awk 'length>=3' \
  | grep -Ev "$stop" \
  | sort -u)

# Pull the description out of YAML frontmatter, folding a `>`/`|` block scalar
# (multi-line, indented) into a single line so it renders in the table.
extract_description() {
  awk '
    /^description:/ {
      val = $0; sub(/^description:[ \t]*/, "", val)
      if (val == ">" || val == "|" || val == ">-" || val == "|-") { block=1; next }
      print val; exit
    }
    block {
      if ($0 ~ /^[ \t]+/) { gsub(/^[ \t]+/, "", $0); buf = buf (buf?" ":"") $0; next }
      print buf; exit
    }
    END { if (block && buf) print buf }
  ' "$1" | tr -d '"'
}

# Name-weighted relevance of one (name, description) pair, mirroring the engine.
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

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

while IFS= read -r f; do
  dir=$(basename "$(dirname "$f")")
  name=$(grep -m1 '^name:' "$f" 2>/dev/null | sed 's/^name: *//; s/ *$//' | tr -d '"' || true)
  name="${name:-$dir}"
  desc=$(extract_description "$f")
  s=$(score_one "$name" "$desc")
  # tab-separated: score \t name \t description (truncated for table use)
  printf '%s\t%s\t%s\n' "$s" "$name" "${desc:0:80}" >>"$tmp"
done < <(find "$skills_dir" -name SKILL.md -type f 2>/dev/null | sort)

# Highest score first; emit JSON lines.
sort -t$'\t' -k1,1nr "$tmp" | while IFS=$'\t' read -r s name desc; do
  esc_desc=${desc//\"/\\\"}
  printf '{"name":"%s","score":%s,"description":"%s","installed":true}\n' \
    "$name" "$s" "$esc_desc"
done
