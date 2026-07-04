#!/usr/bin/env python3
"""
Example webhook sender for testing the aish webhook receiver.

Usage:
    python examples/send_webhook.py --source github --event push --data '{"ref": "refs/heads/main"}'
"""

import argparse
import hmac
import hashlib
import json
import os
import sys
import requests
from datetime import datetime

def sign_webhook(secret: str, body: str) -> str:
    """Generate HMAC-SHA256 signature for webhook body."""
    return hmac.new(
        secret.encode(),
        body.encode(),
        hashlib.sha256
    ).hexdigest()

def send_webhook(url: str, source: str, event: str, data: dict, secret: str = None):
    """Send a webhook to the receiver."""
    
    payload = {
        "event": event,
        "timestamp": int(datetime.now().timestamp()),
        "data": data
    }
    
    body = json.dumps(payload, separators=(',', ':'))
    
    headers = {
        "Content-Type": "application/json",
    }
    
    # Add signature if secret provided
    if secret:
        signature = sign_webhook(secret, body)
        headers["X-Webhook-Signature"] = signature
        print(f"✓ Signature: {signature}")
    else:
        print("⚠ No signature provided")
    
    print(f"→ Sending webhook to {url}/webhooks/{source}")
    print(f"  Event: {event}")
    print(f"  Payload: {body}")
    
    try:
        response = requests.post(
            f"{url}/webhooks/{source}",
            data=body,
            headers=headers,
            timeout=10
        )
        
        if response.status_code == 202:
            result = response.json()
            print(f"✓ Accepted!")
            print(f"  ID: {result.get('id')}")
            print(f"  Received: {result.get('received')}")
            print(f"  Signature Valid: {result.get('signature_valid')}")
            return True
        else:
            print(f"✗ Failed: {response.status_code}")
            print(f"  {response.text}")
            return False
            
    except Exception as e:
        print(f"✗ Error: {e}")
        return False

def list_webhooks(url: str, source: str):
    """List webhooks for a source."""
    print(f"→ Fetching webhooks from {url}/webhooks/{source}")
    
    try:
        response = requests.get(f"{url}/webhooks/{source}", timeout=10)
        
        if response.status_code == 200:
            result = response.json()
            print(f"✓ Found {result['count']} webhook(s)")
            for webhook in result['webhooks']:
                print(f"\n  ID: {webhook['id']}")
                print(f"  Event: {webhook['event']}")
                print(f"  Received: {webhook['received_at']}")
                print(f"  Signature Valid: {webhook['signature_valid']}")
            return True
        else:
            print(f"✗ Failed: {response.status_code}")
            return False
            
    except Exception as e:
        print(f"✗ Error: {e}")
        return False

def health_check(url: str):
    """Check if receiver is healthy."""
    try:
        response = requests.get(f"{url}/health", timeout=5)
        if response.status_code == 200:
            result = response.json()
            print(f"✓ Receiver is healthy (v{result['version']})")
            return True
        else:
            print(f"✗ Receiver unhealthy: {response.status_code}")
            return False
    except Exception as e:
        print(f"✗ Connection failed: {e}")
        return False

if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Send webhooks to aish webhook receiver"
    )
    parser.add_argument(
        "--url",
        default=os.environ.get("WEBHOOK_URL", "http://localhost:8080"),
        help="Receiver URL (default: $WEBHOOK_URL or http://localhost:8080)"
    )
    parser.add_argument(
        "--source",
        required=True,
        help="Webhook source (e.g., github, stripe, slack)"
    )
    parser.add_argument(
        "--event",
        required=True,
        help="Event type (e.g., push, payment, message)"
    )
    parser.add_argument(
        "--data",
        default="{}",
        help="JSON event data (default: {})"
    )
    parser.add_argument(
        "--secret",
        default=os.environ.get("WEBHOOK_SECRET"),
        help="Secret key for signature (default: $WEBHOOK_SECRET)"
    )
    parser.add_argument(
        "--list",
        action="store_true",
        help="List webhooks instead of sending"
    )
    parser.add_argument(
        "--health",
        action="store_true",
        help="Check health instead of sending"
    )
    
    args = parser.parse_args()
    
    if args.health:
        sys.exit(0 if health_check(args.url) else 1)
    
    if args.list:
        sys.exit(0 if list_webhooks(args.url, args.source) else 1)
    
    try:
        data = json.loads(args.data)
    except json.JSONDecodeError as e:
        print(f"✗ Invalid JSON data: {e}")
        sys.exit(1)
    
    success = send_webhook(args.url, args.source, args.event, data, args.secret)
    sys.exit(0 if success else 1)
