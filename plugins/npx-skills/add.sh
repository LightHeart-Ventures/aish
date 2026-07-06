#!/usr/bin/env bash
# npx-skills skill-source: :skill add handler.
#
# Contract (docs/design/plugin-skill-sources.md §3.1):
#   in:  env AISH_SKILL_REF (the reference), AISH_SKILLS_DIR (target skills dir)
#   out: EITHER the raw SKILL.md text (single skill)
#        OR a JSON array of { "path": "<name>", "content": "<SKILL.md>" } (multi)
#   non-zero exit => error (surfaced to the user)
#
# The reference is one this source `handles` — `npx:<spec>` or `skills:<spec>`,
# where <spec> is whatever `npx skills add` accepts, e.g.
#   npx:owner/repo/skill-name         -> npx skills add owner/repo --skill skill-name
#   npx:https://github.com/o/r@ref     -> npx skills add https://github.com/o/r
# We install into a scratch dir, then emit the produced SKILL.md(s) as the
# {path,content} JSON array the façade imports — we never write into the real
# skills dir ourselves (the REPL owns the write + catalog reload).
set -euo pipefail

ref="${AISH_SKILL_REF:-}"
if [ -z "$ref" ]; then
  echo "npx-skills: empty reference" >&2
  exit 1
fi

command -v npx >/dev/null 2>&1 || { echo "npx-skills: 'npx' not found on PATH" >&2; exit 1; }
command -v jq  >/dev/null 2>&1 || { echo "npx-skills: 'jq' not found on PATH" >&2; exit 1; }

# Strip the routing namespace prefix (npx: / skills:) to get the raw spec.
spec="${ref#npx:}"
spec="${spec#skills:}"

# Translate `owner/repo/skill` into `owner/repo --skill skill`; pass URLs/other
# shapes through verbatim.
add_args=()
if [[ "$spec" == http*://* ]]; then
  add_args=("$spec")
else
  IFS='/' read -r -a parts <<< "$spec"
  if [ "${#parts[@]}" -ge 3 ]; then
    repo="${parts[0]}/${parts[1]}"
    skill="${parts[*]:2}"; skill="${skill// /\/}"
    add_args=("$repo" "--skill" "$skill")
  else
    add_args=("$spec")
  fi
fi

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

# Install into the scratch dir. `--dir`/cwd conventions vary; we cd into scratch
# and let the CLI drop its `skills/` tree there.
( cd "$scratch" && npx --yes skills add "${add_args[@]}" >/dev/null 2>&1 ) || {
  echo "npx-skills: 'npx skills add ${add_args[*]}' failed" >&2
  exit 1
}

# Collect every SKILL.md the install produced and emit as {path,content} JSON.
mapfile -t files < <(find "$scratch" -type f -name 'SKILL.md' 2>/dev/null)
if [ "${#files[@]}" -eq 0 ]; then
  echo "npx-skills: no SKILL.md produced for '$ref'" >&2
  exit 1
fi

for f in "${files[@]}"; do
  # Name the skill after its containing directory.
  name="$(basename "$(dirname "$f")")"
  jq -n --arg p "$name" --rawfile c "$f" '{path: $p, content: $c}'
done | jq -s '.'
