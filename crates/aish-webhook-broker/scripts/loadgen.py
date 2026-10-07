#!/usr/bin/env python3
"""aish-webhook-broker load generator (TASK-375). Stdlib only, non-gating.

Sends HMAC-SHA256-signed webhook POSTs to a running broker at a fixed rate,
then prints the achieved rate, HTTP status counts and POST latency.

    # register a poll client with a secret, then 50 req/s for 30 s
    python3 crates/aish-webhook-broker/scripts/loadgen.py \\
        --url http://localhost:8080 --tenant acme --plugin github \\
        --secret s3cret --register --rate 50 --duration 30

Watch the effect in `GET /stats` or the SigNoz dashboard
(deploy/signoz/broker-dashboard.json). Queue-cap drops show up as
`aish.webhook.broker.dropped` once --rate x --duration exceeds
BROKER_MAX_QUEUE_SIZE with no consumer attached.
"""

import argparse
import hashlib
import hmac
import json
import threading
import time
import urllib.error
import urllib.request
import uuid
from collections import Counter


def post(url, body, headers, timeout):
    req = urllib.request.Request(url, data=body, headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            resp.read()
            return resp.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception as e:  # connection refused, timeout, ...
        return type(e).__name__


def register(base, tenant, plugin, secret, timeout):
    body = {"tenant_id": tenant, "plugin_id": plugin,
            "session_id": "loadgen-" + uuid.uuid4().hex[:8], "transport": "poll"}
    if secret:
        body["secret"] = secret
    status = post(base + "/clients/register", json.dumps(body).encode(),
                  {"Content-Type": "application/json"}, timeout)
    print(f"register {tenant}/{plugin}: {status}")
    return status in (200, 201)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--url", default="http://localhost:8080", help="broker base URL")
    ap.add_argument("--tenant", default="loadtest")
    ap.add_argument("--plugin", default="loadgen")
    ap.add_argument("--secret", default="", help="HMAC secret registered for tenant/plugin")
    ap.add_argument("--event-type", default="loadgen.ping")
    ap.add_argument("--rate", type=float, default=10.0, help="target requests per second")
    ap.add_argument("--duration", type=float, default=10.0, help="seconds to run")
    ap.add_argument("--concurrency", type=int, default=4, help="sender threads")
    ap.add_argument("--payload-bytes", type=int, default=256, help="approx payload size")
    ap.add_argument("--timeout", type=float, default=5.0, help="per-request timeout (s)")
    ap.add_argument("--register", action="store_true",
                    help="register a poll client (with --secret) before sending")
    args = ap.parse_args()

    base = args.url.rstrip("/")
    if args.register and not register(base, args.tenant, args.plugin, args.secret, args.timeout):
        raise SystemExit("registration failed")

    target = f"{base}/webhooks/{args.tenant}/{args.plugin}"
    total = max(1, int(args.rate * args.duration))
    interval = 1.0 / args.rate if args.rate > 0 else 0.0
    start = time.monotonic()
    next_idx = [0]
    lock = threading.Lock()
    statuses = Counter()
    latencies = []

    def worker():
        while True:
            with lock:
                i = next_idx[0]
                if i >= total:
                    return
                next_idx[0] += 1
            # Fixed-rate schedule: request i is due at start + i * interval.
            delay = start + i * interval - time.monotonic()
            if delay > 0:
                time.sleep(delay)
            body = json.dumps({"seq": i, "action": "ping", "sent_at": time.time(),
                               "pad": "x" * max(0, args.payload_bytes - 64)}).encode()
            headers = {"Content-Type": "application/json", "X-Event-Type": args.event_type}
            if args.secret:
                sig = hmac.new(args.secret.encode(), body, hashlib.sha256).hexdigest()
                headers["X-Signature"] = "sha256=" + sig
            t0 = time.monotonic()
            status = post(target, body, headers, args.timeout)
            dt = (time.monotonic() - t0) * 1000
            with lock:
                statuses[status] += 1
                latencies.append(dt)

    threads = [threading.Thread(target=worker, daemon=True) for _ in range(max(1, args.concurrency))]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    elapsed = time.monotonic() - start
    latencies.sort()

    def pct(p):
        return latencies[min(len(latencies) - 1, int(p / 100 * len(latencies)))] if latencies else 0.0

    print(f"sent {len(latencies)} to {target} in {elapsed:.1f}s "
          f"({len(latencies) / elapsed:.1f} req/s, target {args.rate:g})")
    print("status: " + ", ".join(f"{k}={v}" for k, v in sorted(statuses.items(), key=str)))
    print(f"latency ms: p50={pct(50):.1f} p99={pct(99):.1f} max={latencies[-1] if latencies else 0:.1f}")
    if any(k != 202 for k in statuses):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
