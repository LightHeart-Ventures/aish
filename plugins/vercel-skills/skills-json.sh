#!/usr/bin/env bash
# vercel-skills plugin — thin bash entrypoint over the zero-dependency Node fork.
# Keeps the plugin invokable as a plain command while the real logic lives in
# bin/skills-json.mjs (Node >= 18, no npm install required).
#
#   ./skills-json.sh list ~/.aish/skills
#   ./skills-json.sh find postgres
#   ./skills-json.sh use sprint-status --include-body
#
# Exit codes propagate from the Node process. Requires `node` on PATH.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! command -v node >/dev/null 2>&1; then
  echo "vercel-skills: 'node' (>=18) not found on PATH" >&2
  exit 127
fi

exec node "$here/bin/skills-json.mjs" "$@"
