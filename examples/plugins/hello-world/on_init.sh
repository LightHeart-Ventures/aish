#!/usr/bin/env bash
# hello-world lifecycle hook (Phase 0.5.4 demo).
#
# A lifecycle hook runs fork/exec at a well-defined moment (here: `on_init`,
# before the REPL starts). aish captures this script's STDOUT and parses any
# `KEY=VALUE` lines into the session environment. Non-KEY=VALUE lines are
# ignored. NEVER emit credential values here — session-env injection redacts /
# rejects secret-looking payloads at the source.
echo "HELLO_WORLD_GREETING=Hello from the hello-world plugin"
echo "HELLO_WORLD_READY=1"
