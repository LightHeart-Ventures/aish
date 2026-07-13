#!/bin/sh
# hello-world broker webhook handler (SPR-069 canonical `webhooks[]` schema).
#
# Contract — identical to the github reference plugin:
#   * fork/exec'd as argv by the aish-webhook-client dispatcher (NO shell wraps
#     the dispatch loop; this script's own shebang is fine),
#   * the broker-delivered event payload arrives as JSON on stdin,
#   * WEBHOOK_* env vars describe the event,
#   * emit ONE concise line on stdout. The client distills it (first non-empty
#     line, capped at 60 chars) and flashes it on the SecondStatusLine.
#
# This is the last hop of the goal: "hello-world plugin receives a webhook →
# message on the statusline."
printf '👋 hello-world: %s webhook received\n' "${WEBHOOK_EVENT_TYPE:-event}"
