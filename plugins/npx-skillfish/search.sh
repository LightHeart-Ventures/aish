#!/usr/bin/env bash
# npx-skillfish skill-source: :skill search handler.
#
# Contract (docs/design/plugin-skill-sources.md §3.1):
#   in:  env AISH_SKILL_QUERY (the query), AISH_SKILL_LIMIT (optional cap)
#   out: JSON array of SearchResult objects on stdout:
#          [ { "name", "author", "description", "version", "reference", "stars" }, ... ]
#   non-zero exit => error (skipped in the search fan-out; never fatal)
#
# This wraps the community `npx skillfish` CLI (skill.fish ecosystem). Provides
# semantic search over the skillfish catalog. Fail-soft: any missing dependency,
# non-JSON output, or CLI error yields an empty result array (`[]`) and exit 0
# so `:skill search` degrades cleanly to its other sources rather than erroring.
set -uo pipefail

emit_empty() { printf '[]\n'; exit 0; }

query="${AISH_SKILL_QUERY:-}"
[ -z "$query" ] && emit_empty

# Require the tools we depend on; degrade to [] if absent.
command -v npx >/dev/null 2>&1 || emit_empty
command -v jq  >/dev/null 2>&1 || emit_empty

# Ask the CLI for JSON. `npx skillfish search <string> --json` prints an array
# or object of catalog entries; shapes vary across versions, so we normalise
# defensively with jq and map whatever fields exist onto the SearchResult schema.
# Unknown => sensible blank.
raw="$(npx --yes skillfish search "$query" --json 2>/dev/null)" || emit_empty
[ -z "$raw" ] && emit_empty

# Validate it parses as JSON; otherwise degrade.
echo "$raw" | jq -e . >/dev/null 2>&1 || emit_empty

# Normalise: accept either a bare array or `{ "results": [...] }` / `{ "skills": [...] }`.
echo "$raw" | jq '
  ( if type == "array" then .
    elif has("results") then .results
    elif has("skills")  then .skills
    else [] end )
  | map({
      name:        ( .name // .slug // .id // "" ),
      author:      ( .author // .owner // .publisher // "" ),
      description: ( .description // .summary // .tagline // "" ),
      version:     ( .version // "" ),
      reference:   ( "skillfish:" + ( .reference // .slug // .full_name // .id // .name // "" ) ),
      stars:       ( .stars // .github_stars // 0 )
    })
' 2>/dev/null || emit_empty
