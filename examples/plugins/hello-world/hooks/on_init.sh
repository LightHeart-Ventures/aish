#!/usr/bin/env bash
# hello-world lifecycle hook (Phase 0.5.4 demo).
#
# A lifecycle hook runs fork/exec at a well-defined moment. This one is
# `on_init` — it fires once at aish startup, BEFORE the REPL prompt appears.
# aish captures this script's STDOUT and parses any `KEY=VALUE` lines into the
# session environment, so every command you subsequently spawn sees them.
#
# Rules the loader enforces:
#   * Only `NAME=VALUE` lines are parsed; NAME must be [A-Za-z_][A-Za-z0-9_]*.
#   * Any other stdout (status text, banners, comments) is ignored — so it's
#     safe to print human-readable progress alongside your exports.
#   * Credential-like keys (containing secret/password/token/key/…) are
#     REJECTED with a warning. NEVER emit secrets here — route them through a
#     credential profile instead.
#   * Ambient/user env wins on a clash; the alphabetically-first plugin wins
#     between plugins.
#
# Disable this mechanism entirely with:  AISH_ENV_INJECTION_DISABLED=1

echo "hello-world: on_init running"          # ignored (not KEY=VALUE)
echo "EXAMPLE_VAR=value"                       # injected → $EXAMPLE_VAR
echo "HELLO_WORLD_GREETING=Hello from the hello-world plugin"
echo "HELLO_WORLD_READY=1"

# The line below would be REJECTED by the loader (credential-like key) and
# never reaches the session env — shown here only to demonstrate the guardrail:
# echo "HELLO_WORLD_API_TOKEN=nope"
