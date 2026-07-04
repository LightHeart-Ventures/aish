#!/usr/bin/env bash
# github plugin — on_shell_ready lifecycle hook.
#
# Fires once after the REPL is fully initialized and interactive. Unlike
# on_init, its STDOUT is NOT parsed for env injection — it's for side effects
# (warming a cache, a one-line readiness banner to STDERR, registering state).
# Keep it fast and non-blocking; a slow hook here delays the first prompt.
set -euo pipefail

owner="${GITHUB_OWNER:-<unset>}"
repo="${GITHUB_REPO:-<unset>}"

# Human-readable banner to STDERR (STDOUT stays clean by convention).
echo "github plugin ready — target repo: ${owner}/${repo}" >&2

# A real hook might prime the MCP connection or pre-fetch open-PR counts here.
exit 0
