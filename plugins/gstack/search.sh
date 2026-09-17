#!/usr/bin/env bash
# gstack skill-source: `:skill search` handler.
#
# Contract (docs/design/plugin-skill-sources.md §3.1):
#   in:  env AISH_SKILL_QUERY (the query), AISH_SKILL_LIMIT (optional cap)
#   out: JSON array of SearchResult objects on stdout:
#          [ { "name", "author", "description", "version", "reference", "stars" }, ... ]
#   non-zero exit => error (skipped in the search fan-out; never fatal)
#
# Search is served from the bundled offline catalog.json (no network, no rate
# limit, instant). Refresh it against upstream with ./refresh-catalog.sh.
# Fail-soft: anything missing yields `[]` + exit 0 so `:skill search` degrades
# cleanly to its other sources.
set -uo pipefail

emit_empty() { printf '[]\n'; exit 0; }

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
catalog="${AISH_GSTACK_CATALOG:-$here/catalog.json}"
query="${AISH_SKILL_QUERY:-}"
limit="${AISH_SKILL_LIMIT:-50}"

command -v jq >/dev/null 2>&1 || emit_empty
[ -r "$catalog" ] || emit_empty
case "$limit" in ''|*[!0-9]*) limit=50 ;; esac

# Empty query => whole catalog (the `:skill search` UI uses that as a browse).
# Otherwise substring-match (case-insensitive) on name + description.
jq -c --arg q "$query" --argjson limit "$limit" '
  ( $q | ascii_downcase ) as $needle
  | map(select(
      $needle == ""
      or ( (.name        // "") | ascii_downcase | contains($needle) )
      or ( (.description // "") | ascii_downcase | contains($needle) )
      or ( (.dir         // "") | ascii_downcase | contains($needle) )
    ))
  | map({
      name:        .name,
      author:      "garrytan",
      description: .description,
      version:     ( .version // "" ),
      reference:   ( "gstack:" + .name ),
      stars:       0
    })
  | .[0:$limit]
' "$catalog" 2>/dev/null || emit_empty
