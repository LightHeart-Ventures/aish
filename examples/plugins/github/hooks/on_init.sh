#!/usr/bin/env bash
# github plugin — on_init lifecycle hook.
#
# Fires once at aish startup, BEFORE the REPL prompt. aish captures STDOUT and
# parses `KEY=VALUE` lines into the session environment (see hello-world's
# on_init.sh and docs/PLUGIN_SYSTEM_DESIGN.md §0.5.4). Non-KEY=VALUE stdout is
# ignored; credential-like keys are rejected. NEVER emit a token here — the
# GitHub PAT is resolved from [profile:github] at MCP connect time.

echo "github: on_init — resolving repo context"

# Surface the configured repo to the session so skills/tools can default to it.
# These fall back to the plugin config / ambient env; ambient env wins on clash.
echo "GITHUB_OWNER=${GITHUB_OWNER:-}"
echo "GITHUB_REPO=${GITHUB_REPO:-}"
echo "GITHUB_PLUGIN_READY=1"
