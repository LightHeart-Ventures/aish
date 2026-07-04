#!/usr/bin/env bash
# install-sysbox.sh — one-shot installer + configurator + verifier for Sysbox CE,
# the nesting-capable OCI runtime that lets an in-container aish coordinator launch
# its own grandchild container worker (see docs/sysbox.md and PR #520).
#
# WHY THIS IS A SEPARATE SUDO SCRIPT:
#   Installing Sysbox is entirely root-level work — it drops a `sysbox` system user,
#   installs three systemd units (sysbox.service / sysbox-mgr / sysbox-fs), rewrites
#   /etc/docker/daemon.json to register the `sysbox-runc` runtime, and restarts
#   dockerd. A headless aish coordinator has no passwordless sudo, so it cannot do
#   any of that itself. Run this once, by hand, with sudo.
#
# USAGE:
#   sudo bash scripts/install-sysbox.sh
#
# IDEMPOTENT: safe to re-run. Skips the download if the verified .deb is cached,
# skips install if the runtime is already registered, and always re-runs the
# end-to-end nested-docker smoke test at the end.
#
# HOST NOTES:
#   * This host is WSL2 (6.x microsoft-standard-WSL2). Sysbox is NOT officially
#     supported on WSL2, BUT the two classic blockers are cleared here: systemd is
#     running (PID1) and the kernel exposes idmapped mounts (so shiftfs isn't
#     needed). It has a real chance of working; the smoke test is the arbiter.
#   * If the smoke test fails on WSL2, that's the officially-unsupported path — do
#     NOT chase it indefinitely; fall back to running heavy nested work on a native
#     Linux host / CI runner where Sysbox is supported.
set -euo pipefail

SYSBOX_VERSION="0.7.0"
DEB="sysbox-ce_${SYSBOX_VERSION}.linux_amd64.deb"
URL="https://github.com/nestybox/sysbox/releases/download/v${SYSBOX_VERSION}/${DEB}"
SHA256="eeff273671467b8fa351ab3d40709759462dc03d9f7b50a1b207b37982ce40a9"
CACHE="/tmp/${DEB}"
RUNTIME="sysbox-runc"

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[fail]\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "must run as root: sudo bash scripts/install-sysbox.sh"

# ── 0. Prereq snapshot ────────────────────────────────────────────────────────
say "Host: $(uname -sr)"
command -v docker >/dev/null || die "docker not found on PATH"
systemctl is-system-running --quiet 2>/dev/null || warn "systemd not fully running — sysbox services may not start cleanly"
if uname -r | grep -qi 'microsoft-standard-WSL2'; then
    warn "WSL2 detected — Sysbox is officially unsupported here. Proceeding because systemd + idmapped mounts are present; the smoke test decides."
fi

# ── 1. Fetch + verify the package ─────────────────────────────────────────────
verify() { echo "${SHA256}  ${CACHE}" | sha256sum -c - >/dev/null 2>&1; }
if [ -f "$CACHE" ] && verify; then
    say "Using cached, checksum-verified ${DEB}"
else
    say "Downloading ${DEB}"
    curl -fsSL -o "$CACHE" "$URL" || die "download failed"
    verify || die "checksum mismatch on ${CACHE} (expected ${SHA256})"
    say "Checksum OK"
fi

# ── 2. Install (apt resolves deps: jq, rsync, fuse, etc.) ─────────────────────
if docker info --format '{{json .Runtimes}}' 2>/dev/null | grep -q "\"${RUNTIME}\""; then
    say "Runtime '${RUNTIME}' already registered — skipping package install"
else
    say "Installing ${DEB} (apt will pull dependencies)"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq || warn "apt-get update failed (continuing)"
    apt-get install -y "$CACHE" || die "package install failed"
fi

# ── 3. Bring up sysbox services ───────────────────────────────────────────────
say "Enabling + starting sysbox.service"
systemctl enable sysbox >/dev/null 2>&1 || true
systemctl restart sysbox || die "failed to start sysbox.service — check: journalctl -u sysbox-mgr -u sysbox-fs"
for svc in sysbox-mgr sysbox-fs; do
    systemctl is-active --quiet "$svc" || warn "${svc} not active (journalctl -u ${svc})"
done

# ── 4. Ensure the runtime is registered with dockerd ──────────────────────────
if ! docker info --format '{{json .Runtimes}}' 2>/dev/null | grep -q "\"${RUNTIME}\""; then
    say "Registering '${RUNTIME}' in /etc/docker/daemon.json and restarting docker"
    mkdir -p /etc/docker
    if [ -f /etc/docker/daemon.json ]; then
        cp -a /etc/docker/daemon.json "/etc/docker/daemon.json.bak.$(date +%s)"
    else
        echo '{}' > /etc/docker/daemon.json
    fi
    # Merge the runtime entry with jq (installed as a sysbox dep).
    tmp="$(mktemp)"
    jq --arg bin "$(command -v sysbox-runc)" \
       '.runtimes["sysbox-runc"] = {"path": $bin}' \
       /etc/docker/daemon.json > "$tmp" && mv "$tmp" /etc/docker/daemon.json
    systemctl restart docker || die "docker restart failed after daemon.json edit"
    sleep 2
fi

docker info --format '{{json .Runtimes}}' 2>/dev/null | grep -q "\"${RUNTIME}\"" \
    || die "runtime '${RUNTIME}' still not visible to dockerd — inspect /etc/docker/daemon.json + journalctl -u docker"
say "Runtime '${RUNTIME}' is registered with dockerd"

# ── 5. End-to-end nested-docker smoke test ────────────────────────────────────
# The real proof: a Sysbox container running its OWN dockerd that runs a container.
say "Running nested-docker smoke test (this is the actual proof)…"
docker rm -f sysbox-smoke >/dev/null 2>&1 || true
if docker run --runtime="${RUNTIME}" --name sysbox-smoke -d nestybox/ubuntu-noble-systemd-docker >/dev/null 2>&1; then
    ok=""
    for _ in $(seq 1 24); do
        if docker exec sysbox-smoke docker run --rm hello-world >/dev/null 2>&1; then
            ok=1; break
        fi
        sleep 5
    done
    docker rm -f sysbox-smoke >/dev/null 2>&1 || true
    if [ -n "$ok" ]; then
        say "SMOKE TEST PASSED — nested dockerd ran a container inside a Sysbox container."
    else
        warn "Smoke test container started but inner 'docker run' never succeeded."
        warn "On WSL2 this is the unsupported path. Check: docker logs sysbox-smoke / journalctl -u sysbox-fs"
        SMOKE_FAILED=1
    fi
else
    warn "Could not launch a container under ${RUNTIME}. Check: journalctl -u sysbox-mgr -u sysbox-fs"
    SMOKE_FAILED=1
fi

# ── 6. Opt-in instructions for aish ───────────────────────────────────────────
cat <<'EOF'

────────────────────────────────────────────────────────────────────────────
Sysbox install complete. To make aish route NESTED workers through it, export:

    export AISH_SYSBOX_RUNTIME=sysbox-runc

in the environment the top-level aish coordinator runs in (e.g. add it to your
shell profile or the aish service unit). Without that var, aish's
resolve_runtime_override() returns None and nested spawns use the old
(non-nesting) path — byte-for-byte unchanged. See docs/sysbox.md.
────────────────────────────────────────────────────────────────────────────
EOF

[ -n "${SMOKE_FAILED:-}" ] && exit 3
exit 0
