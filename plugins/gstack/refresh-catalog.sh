#!/usr/bin/env bash
# Regenerate plugins/gstack/catalog.json from upstream garrytan/gstack.
#
# Walks the repo tree at $REF, finds every top-level <dir>/SKILL.md, reads each
# one's YAML frontmatter (name + description), and writes the offline catalog
# that search.sh serves. Run this when upstream adds/renames skills.
#
#   ./refresh-catalog.sh            # against main
#   REF=v1.2.0 ./refresh-catalog.sh # against a tag
#
# Deps: curl, jq. Uses the unauthenticated GitHub API for the tree listing;
# set GITHUB_TOKEN to avoid the 60 req/h anonymous rate limit.
set -euo pipefail

REF="${REF:-main}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:-$here/catalog.json}"

# bash 3.2 (stock macOS) errors on "${arr[@]}" for an empty array under `set -u`,
# so guard the expansion rather than relying on bash 4+ semantics.
auth=()
[ -n "${GITHUB_TOKEN:-}" ] && auth=(-H "Authorization: Bearer $GITHUB_TOKEN")

tree_json="$(curl -fsSL ${auth[@]+"${auth[@]}"} \
  "https://api.github.com/repos/garrytan/gstack/git/trees/${REF}?recursive=1")"

dirs_file="$(mktemp)"
trap 'rm -f "$dirs_file"' EXIT
echo "$tree_json" \
  | jq -r '.tree[].path | select(test("^[^/]+/SKILL\\.md$")) | sub("/SKILL\\.md$";"")' \
  | sort -u > "$dirs_file"

echo "found $(wc -l < "$dirs_file" | tr -d ' ') skills" >&2

tmp="$(mktemp)"
printf '[\n' > "$tmp"
first=1
while IFS= read -r d; do
  [ -z "$d" ] && continue
  md="$(curl -fsSL "https://raw.githubusercontent.com/garrytan/gstack/${REF}/${d}/SKILL.md" || true)"
  [ -z "$md" ] && continue
  # Herestrings, not `printf | awk`: awk's early `exit` SIGPIPEs the writer, and
  # `set -o pipefail` would turn that into a fatal error mid-loop.
  name="$(awk '/^name:/{sub(/^name:[[:space:]]*/,""); print; exit}' <<< "$md")"
  desc="$(awk '/^description:/{sub(/^description:[[:space:]]*/,""); print; exit}' <<< "$md")"
  ver="$(awk '/^version:/{sub(/^version:[[:space:]]*/,""); print; exit}' <<< "$md")"
  # Strip surrounding quotes YAML may carry.
  desc="${desc%\"}"; desc="${desc#\"}"
  ver="${ver%\"}";  ver="${ver#\"}"
  [ -z "$name" ] && name="$d"
  [ -z "$desc" ] && desc="gstack skill: $d"
  # Trim absurdly long descriptions so the search table stays readable.
  if [ "${#desc}" -gt 160 ]; then desc="${desc:0:157}..."; fi
  [ $first -eq 0 ] && printf ',\n' >> "$tmp"
  jq -nc --arg n "$name" --arg d "$desc" --arg v "$ver" --arg dir "$d" \
    '{name:$n, description:$d, version:$v, dir:$dir}' >> "$tmp"
  first=0
  echo "  + $d" >&2
done < "$dirs_file"
printf '\n]\n' >> "$tmp"

jq '.' "$tmp" > "$out"
rm -f "$tmp"
echo "wrote $out" >&2
