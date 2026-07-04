#!/usr/bin/env bash
# github plugin — `login` handler (backs `aish login github`).
#
# aish runs this when the user authenticates the plugin. Its job: obtain a
# GitHub token and persist it to the [profile:github] credential section that
# .mcp.json references via ${profile:github.access_token}. aish provides the
# secure write; this script only sources the value and prints the key(s) to
# persist as `KEY=VALUE` lines on STDOUT (captured into the profile, NOT echoed
# to the terminal or logs).
#
# Two supported paths:
#   1. Ambient PAT  — $GITHUB_TOKEN / $GH_TOKEN already in the environment.
#   2. gh CLI       — reuse an existing `gh auth` session's token.
set -euo pipefail

token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"

if [[ -z "${token}" ]] && command -v gh >/dev/null 2>&1; then
  token="$(gh auth token 2>/dev/null || true)"
fi

if [[ -z "${token}" ]]; then
  echo "github login: no token found. Set GITHUB_TOKEN or run 'gh auth login' first." >&2
  exit 1
fi

# Persisted into [profile:github]; resolved later as ${profile:github.access_token}.
echo "access_token=${token}"
