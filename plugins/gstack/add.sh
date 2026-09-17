#!/usr/bin/env bash
# gstack skill-source: `:skill add` handler.
#
# Contract (docs/design/plugin-skill-sources.md §3.1):
#   in:  env AISH_SKILL_REF (the reference), AISH_SKILLS_DIR
#   out: stdout is EITHER a raw SKILL.md (single skill)
#        OR a JSON array of { "path": "<name>", "content": "<SKILL.md text>" }
#        (multi-skill import)
#   non-zero exit => error (surfaced to the user)
#
# Accepted references:
#   gstack:ship                 -> skills/ship/SKILL.md  (one skill)
#   gstack:gstack               -> the top-level gstack router skill
#   gstack:*  | gstack:all      -> every skill in the catalog (multi-import)
#   garrytan/gstack/ship        -> same as gstack:ship
#   garrytan/gstack             -> full suite
#
# Fetches from raw.githubusercontent.com at $AISH_GSTACK_REF (default `main`).
set -uo pipefail

die() { printf 'gstack: %s\n' "$1" >&2; exit 1; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
catalog="${AISH_GSTACK_CATALOG:-$here/catalog.json}"
ref="${AISH_GSTACK_REF:-main}"
raw_base="https://raw.githubusercontent.com/garrytan/gstack/${ref}"

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v jq   >/dev/null 2>&1 || die "jq is required"
[ -r "$catalog" ] || die "catalog not found: $catalog (run ./refresh-catalog.sh)"

input="${AISH_SKILL_REF:-}"
[ -z "$input" ] && die "no skill reference supplied"

# Normalise: strip the source prefix / owner path.
name="$input"
name="${name#gstack:}"
name="${name#garrytan/gstack/}"
[ "$name" = "garrytan/gstack" ] && name="*"
[ "$name" = "gstack" ] && name="__router__"

# Resolve a catalog name -> upstream SKILL.md path.
skill_path() {
  local n="$1"
  if [ "$n" = "__router__" ]; then
    printf 'SKILL.md'
    return 0
  fi
  jq -r --arg n "$n" '
    map(select(.name == $n or .dir == $n)) | .[0].dir // empty
  ' "$catalog" | while IFS= read -r d; do
    [ -n "$d" ] && printf '%s/SKILL.md' "$d"
  done
}

fetch() {
  curl -fsSL --retry 2 --max-time 30 "$raw_base/$1" \
    || die "fetch failed: $raw_base/$1 (ref=$ref)"
}

if [ "$name" = "*" ] || [ "$name" = "all" ]; then
  # Multi-import: every catalog entry + the router skill.
  tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
  printf '[' > "$tmp"
  first=1
  # Router first so `gstack` itself is always present.
  content="$(fetch 'SKILL.md')"
  printf '%s' "$(jq -n --arg p gstack --arg c "$content" '{path:$p,content:$c}')" >> "$tmp"
  first=0
  while IFS= read -r n; do
    [ -z "$n" ] && continue
    p="$(skill_path "$n")"
    [ -z "$p" ] && continue
    content="$(fetch "$p")" || continue
    [ $first -eq 0 ] && printf ',' >> "$tmp"
    printf '%s' "$(jq -n --arg p "$n" --arg c "$content" '{path:$p,content:$c}')" >> "$tmp"
    first=0
  done < <(jq -r '.[].name' "$catalog")
  printf ']' >> "$tmp"
  cat "$tmp"
  exit 0
fi

path="$(skill_path "$name")"
[ -z "$path" ] && die "unknown gstack skill: '$name' (try: :skill search gstack)"
fetch "$path"
