#!/bin/bash
# Test: Send a hello-world webhook through the broker and capture statusline output

set -e

BROKER_URL="https://aish-webhook-broker.fly.dev"

echo "Sending hello-world webhook test to broker..."
curl -s -X POST "$BROKER_URL/v1/events" \
  -H "Content-Type: application/json" \
  -d '{
    "event_type": "hello-world-statusline-test",
    "data": {
      "message": "Hello, World! from webhook broker",
      "plugin": "hello-world",
      "timestamp": "'$(date -Iseconds)'"
    }
  }' || true

echo "✓ Webhook queued at broker"
echo ""
echo "To see this message rendered in the aish statusline:"
echo "  1. Set WEBHOOK_BROKER_URL=$BROKER_URL"
echo "  2. Run 'aish' with the hello-world plugin enabled"
echo "  3. The plugin's webhook_command will execute and flash: 👋 Hello World plugin received: hello-world-statusline-test"
echo ""
echo "Recent broker log (webhooks received):"
flyctl logs -a aish-webhook-broker -n | grep "webhook received" | tail -5
