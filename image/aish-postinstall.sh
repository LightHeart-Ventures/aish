#!/bin/sh
# aish-postinstall.sh — turn a fresh, bare-bones Debian system into one whose
# default login shell is aish.
#
# This runs INSIDE the newly installed system (invoked from preseed's
# `preseed/late_command` via `in-target`, or manually on any Debian/Ubuntu box).
# It is deliberately POSIX /bin/sh so it works in the stripped installer target
# before any richer shell exists.
#
# What it does:
#   1. installs aish's runtime deps (libgomp1 etc.) + ca-certificates + git
#   2. fetches the pinned aish release binary, verifies its SHA256
#   3. installs it to /usr/local/bin/aish and registers it in /etc/shells
#   4. sets aish as the login shell for the target user AND root
#   5. seeds /etc/profile.d/aish.sh + /etc/skel/.aishrc so the API key is picked up
#
# Tunables (env):
#   AISH_VERSION   git tag of the release to install         (default: v0.27.0)
#   AISH_USER      login user to switch to aish              (default: aish)
#   AISH_REPO      github owner/repo                          (default: LightHeart-Ventures/aish)
#   AISH_SET_ROOT  also switch root's shell to aish (1/0)     (default: 1)
set -eu

AISH_VERSION="${AISH_VERSION:-v0.27.0}"
AISH_USER="${AISH_USER:-aish}"
AISH_REPO="${AISH_REPO:-LightHeart-Ventures/aish}"
AISH_SET_ROOT="${AISH_SET_ROOT:-1}"
AISH_BIN=/usr/local/bin/aish

log() { printf '[aish-postinstall] %s\n' "$*" >&2; }
die() { log "ERROR: $*"; exit 1; }

# ---- 0. arch guard -------------------------------------------------------
# The published release binary is glibc x86_64 only. On other arches we bail
# with a clear message rather than installing a binary that can't exec.
arch="$(uname -m)"
case "$arch" in
  x86_64|amd64) asset="aish-x86_64-unknown-linux-gnu" ;;
  *) die "no prebuilt aish release for arch '$arch' — build from source (see scripts/install-ubuntu-24.04.sh) or add a musl/arm64 target to the release matrix." ;;
esac

# ---- 1. runtime dependencies --------------------------------------------
# libstdc++6 / libgcc-s1 / libc / libm ship in the Debian base system; libgomp1
# (OpenMP, pulled in by aish's `local` llama.cpp feature) does not. git +
# ca-certificates are needed by the coordinator + for TLS to the Anthropic API.
log "installing runtime dependencies"
export DEBIAN_FRONTEND=noninteractive
if command -v apt-get >/dev/null 2>&1; then
  apt-get update -qq || true
  apt-get install -y --no-install-recommends \
    libgomp1 libstdc++6 libgcc-s1 ca-certificates git wget \
    || die "apt-get install of runtime deps failed"
else
  log "WARN: apt-get not found — assuming deps (libgomp1, ca-certificates, git) are already present"
fi

# ---- 2. fetch + verify the binary ---------------------------------------
base="https://github.com/${AISH_REPO}/releases/download/${AISH_VERSION}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fetch() { # url dest
  if command -v wget >/dev/null 2>&1; then wget -q -O "$2" "$1"
  elif command -v curl >/dev/null 2>&1; then curl -fsSL -o "$2" "$1"
  else die "neither wget nor curl available to download aish"; fi
}

log "downloading ${asset} @ ${AISH_VERSION}"
fetch "${base}/${asset}"          "${tmp}/aish"        || die "download of ${asset} failed"
fetch "${base}/${asset}.sha256"   "${tmp}/aish.sha256" || die "download of checksum failed"

log "verifying SHA256"
# The .sha256 file is `<hash>  <asset-name>`; check it against our downloaded copy.
expected="$(awk '{print $1}' "${tmp}/aish.sha256")"
actual="$(sha256sum "${tmp}/aish" | awk '{print $1}')"
[ -n "$expected" ] || die "empty expected checksum"
[ "$expected" = "$actual" ] || die "checksum mismatch: expected $expected got $actual"
log "checksum OK ($actual)"

# ---- 3. install + register in /etc/shells -------------------------------
install -m 0755 "${tmp}/aish" "$AISH_BIN"
log "installed $($AISH_BIN --version 2>/dev/null || echo aish) -> $AISH_BIN"

if ! grep -qxF "$AISH_BIN" /etc/shells 2>/dev/null; then
  echo "$AISH_BIN" >> /etc/shells
  log "registered $AISH_BIN in /etc/shells"
fi

# ---- 4. make aish the default login shell -------------------------------
set_shell() { # username
  user="$1"
  if id "$user" >/dev/null 2>&1; then
    chsh -s "$AISH_BIN" "$user" 2>/dev/null \
      || usermod -s "$AISH_BIN" "$user" 2>/dev/null \
      || { log "WARN: could not set shell for $user"; return 0; }
    log "default shell for $user -> $AISH_BIN"
  else
    log "WARN: user '$user' does not exist — skipping"
  fi
}
set_shell "$AISH_USER"
[ "$AISH_SET_ROOT" = "1" ] && set_shell root

# Also make aish the default for future-created users.
if [ -f /etc/adduser.conf ]; then
  if grep -q '^DSHELL=' /etc/adduser.conf; then
    sed -i "s|^DSHELL=.*|DSHELL=$AISH_BIN|" /etc/adduser.conf
  else
    echo "DSHELL=$AISH_BIN" >> /etc/adduser.conf
  fi
fi

# ---- 5. seed config ------------------------------------------------------
# Login-shell environment: source the API key from a private, root-owned drop
# file if present, otherwise nudge the user to set it.
cat > /etc/profile.d/aish.sh <<'PROFILE'
# aish environment — sourced by login shells.
# Put your key in /etc/aish/env (chmod 600) or export it in ~/.aishrc.
if [ -r /etc/aish/env ]; then
  . /etc/aish/env
fi
if [ -z "${ANTHROPIC_API_KEY:-}" ] && [ -z "${AISH_QUIET_KEY_WARN:-}" ]; then
  printf '\033[1;33maish:\033[0m ANTHROPIC_API_KEY is not set. Run: export ANTHROPIC_API_KEY=sk-ant-...\n' >&2
fi
PROFILE
chmod 0644 /etc/profile.d/aish.sh

mkdir -p /etc/aish
if [ ! -f /etc/aish/env ]; then
  cat > /etc/aish/env <<'ENVDROP'
# aish secrets — populate and `chmod 600 /etc/aish/env`.
# export ANTHROPIC_API_KEY=sk-ant-...
ENVDROP
  chmod 0600 /etc/aish/env
fi

# Per-user rc skeleton so new accounts get a sane starting point.
cat > /etc/skel/.aishrc <<'RC'
# ~/.aishrc — aish startup config (sourced at REPL launch).
# export ANTHROPIC_API_KEY=sk-ant-...
# :mode careful      # confirmation gate: paranoid|careful|normal|yolo
RC

log "done — aish is the default shell for ${AISH_USER}$([ "$AISH_SET_ROOT" = 1 ] && echo ' and root')."
