#!/bin/bash
# Quick setup and deploy script for webhook receiver on Fly.io

set -e

WEBHOOK_RECEIVER_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(dirname "$(dirname "$WEBHOOK_RECEIVER_DIR")")"

echo "🚀 Aish Webhook Receiver — Fly.io Setup"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# Check prerequisites
echo ""
echo "Checking prerequisites..."
command -v flyctl >/dev/null 2>&1 || { echo "✗ flyctl not found. Install: https://fly.io/docs/hands-on/install/"; exit 1; }
command -v cargo >/dev/null 2>&1 || { echo "✗ cargo not found"; exit 1; }
echo "✓ flyctl and cargo installed"

# Check if already logged in
echo ""
echo "Checking Fly.io authentication..."
if flyctl auth whoami >/dev/null 2>&1; then
    echo "✓ Already authenticated to Fly.io"
else
    echo "→ You need to log in to Fly.io..."
    flyctl auth login
fi

# Build locally first
echo ""
echo "Building webhook-receiver locally..."
cd "$REPO_ROOT"
cargo build -p webhook-receiver --release || { echo "✗ Build failed"; exit 1; }
echo "✓ Build successful"

# Get or create Fly app
echo ""
echo "Setting up Fly.io app..."
APP_NAME="${1:-aish-webhooks}"
ORG="${FLY_ORG:-personal}"

if flyctl apps list -o "$ORG" | grep -q "^$APP_NAME\s"; then
    echo "✓ App '$APP_NAME' already exists"
else
    echo "→ Creating Fly app '$APP_NAME' in org '$ORG'..."
    flyctl apps create "$APP_NAME" -o "$ORG" || { echo "✗ Failed to create app"; exit 1; }
fi

# Create volume if needed
echo ""
echo "Setting up persistent storage..."
VOLUME_NAME="webhooks_data"
if flyctl volumes list -a "$APP_NAME" | grep -q "^$VOLUME_NAME"; then
    echo "✓ Volume '$VOLUME_NAME' already exists"
else
    echo "→ Creating volume '$VOLUME_NAME'..."
    flyctl volumes create "$VOLUME_NAME" --size 1 --region iad -a "$APP_NAME"
fi

# Set secrets (skip if already configured — avoids blocking on the prompt
# during re-deploys and never re-stages an unnecessary secret change).
echo ""
echo "Setting up secrets..."
if flyctl secrets list -a "$APP_NAME" 2>/dev/null | grep -q "^WEBHOOK_SECRET"; then
    echo "✓ WEBHOOK_SECRET already set (skipping)"
elif [ -t 0 ]; then
    read -sp "Enter webhook secret (press Enter for random): " WEBHOOK_SECRET
    echo ""
    if [ -z "$WEBHOOK_SECRET" ]; then
        WEBHOOK_SECRET=$(openssl rand -hex 32)
        echo "Generated random secret: $WEBHOOK_SECRET"
    fi
    echo "→ Setting WEBHOOK_SECRET..."
    flyctl secrets set WEBHOOK_SECRET="$WEBHOOK_SECRET" -a "$APP_NAME"
    echo "✓ Secret configured"
else
    WEBHOOK_SECRET=$(openssl rand -hex 32)
    echo "Non-interactive; generated random secret: $WEBHOOK_SECRET"
    flyctl secrets set WEBHOOK_SECRET="$WEBHOOK_SECRET" -a "$APP_NAME"
    echo "✓ Secret configured"
fi

# Deploy
echo ""
echo "Deploying to Fly.io..."
cd "$WEBHOOK_RECEIVER_DIR"
# The Docker build context MUST be the workspace root (see Dockerfile header):
# aish is a cargo workspace, so `cargo build -p webhook-receiver` needs every
# member's Cargo.toml present. Point fly at the repo root as the context while
# keeping the config + Dockerfile in-crate.
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
flyctl deploy "$REPO_ROOT" \
  --config "$REPO_ROOT/crates/webhook-receiver/fly.toml" \
  --dockerfile "$REPO_ROOT/crates/webhook-receiver/Dockerfile" \
  --wait-timeout 5m \
  -a "$APP_NAME"

# Get app URL
echo ""
echo "Deployment complete!"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
APP_URL=$(flyctl info -a "$APP_NAME" | grep "Hostname:" | awk '{print $NF}')
echo ""
echo "✓ Webhook Receiver is live!"
echo "  URL: https://$APP_URL"
echo "  Health check: https://$APP_URL/health"
echo ""
echo "Next steps:"
echo "  1. Save your webhook secret (keep it safe!)"
echo "  2. Set up webhook senders to POST to: https://$APP_URL/webhooks/{source}"
echo "  3. Monitor logs: flyctl logs -a $APP_NAME"
echo ""
echo "Example webhook:"
echo "  curl -X POST https://$APP_URL/webhooks/github \\"
echo "    -H 'Content-Type: application/json' \\"
echo "    -d '{\"event\":\"push\",\"data\":{}}'"
echo ""
