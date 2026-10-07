#!/usr/bin/env bash
# Manual Stage C smoke (TASK-449): POST a `ping` webhook for the hello-world
# plugin to a running aish-webhook-broker. A connected aish (see the exports
# printed below) runs plugins/hello-world/handlers/ping.sh and flashes
# "👋 <message>" on the SecondStatusLine.
#
# Usage:
#   BROKER_HTTP=https://aish-webhook-broker.fly.dev \
#   TENANT=default WEBHOOK_BROKER_SECRET=... ./test-webhook-statusline.sh ["message"]
#
# The broker only accepts webhooks for a (tenant, plugin) that some client has
# registered, so start aish FIRST (otherwise this returns 404). If aish
# registered with WEBHOOK_BROKER_SECRET, the same secret must be set here: the
# body is HMAC-SHA256 signed into `X-Signature: sha256=<hex>`.
set -euo pipefail

BROKER_HTTP="${BROKER_HTTP:-https://aish-webhook-broker.fly.dev}"
BROKER_HTTP="${BROKER_HTTP%/}"
TENANT="${TENANT:-default}"
PLUGIN="${PLUGIN:-hello-world}"
MESSAGE="${1:-Hello, World! from webhook broker}"

BODY=$(printf '{"message":"%s"}' "$MESSAGE")
HEADERS=(-H "Content-Type: application/json" -H "X-Event-Type: ping")
if [[ -n "${WEBHOOK_BROKER_SECRET:-}" ]]; then
  SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$WEBHOOK_BROKER_SECRET" | sed 's/^.*= //')
  HEADERS+=(-H "X-Signature: sha256=$SIG")
fi

echo "POST $BROKER_HTTP/webhooks/$TENANT/$PLUGIN  (event ping)"
HTTP_CODE=$(curl -sS -o /tmp/aish-webhook-test.out -w '%{http_code}' \
  -X POST "$BROKER_HTTP/webhooks/$TENANT/$PLUGIN" "${HEADERS[@]}" -d "$BODY")
echo "→ HTTP $HTTP_CODE: $(cat /tmp/aish-webhook-test.out)"

case "$HTTP_CODE" in
  202) echo "✓ queued — a connected aish should flash: 👋 $MESSAGE" ;;
  404) echo "✗ unknown tenant/plugin — start aish with the env below first"; exit 1 ;;
  401) echo "✗ signature rejected — WEBHOOK_BROKER_SECRET must match the one aish registered with"; exit 1 ;;
  *)   echo "✗ unexpected response"; exit 1 ;;
esac

WS_URL="${BROKER_HTTP/https:/wss:}"
WS_URL="${WS_URL/http:/ws:}"
cat <<EOF

aish side (must be running before the POST):
  export WEBHOOK_BROKER_URL=$WS_URL/ws
  export WEBHOOK_PLUGIN_ID=$PLUGIN
  export WEBHOOK_TENANT_ID=$TENANT
  # export WEBHOOK_BROKER_SECRET=...   (optional; then sign as above)
  aish        # then :webhook status / :webhook logs
EOF
