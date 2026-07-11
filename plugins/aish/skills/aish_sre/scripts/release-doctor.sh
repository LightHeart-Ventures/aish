#!/usr/bin/env bash
# release-doctor.sh — reconcile an aish release across the four facts that must
# agree (SKILL.md §0 step 6-7, §1, §2, triage step 3/7):
#   1. the pushed git TAG                (vX.Y.Z)
#   2. Cargo.toml on the TAGGED commit   (verify-version checks this)
#   3. Cargo.toml on origin/main         (the bump must have merged — §2)
#   4. the PUBLISHED release             (isDraft + asset count — §1)
#   5. Cargo.lock ↔ Cargo.toml on main   (the --locked CI gate — §4)
#   6. the locally installed binary      (aish --version — tag↔Cargo drift §2)
#
# Read-only. Never pushes, tags, or deletes. Prints PASS/WARN/FAIL per invariant
# with the SKILL.md section to consult on failure, and exits non-zero if any
# hard invariant failed.
#
# Usage:
#   release-doctor.sh              # inspect the latest release/tag
#   release-doctor.sh v0.20.1      # inspect a specific version
#   REPO=owner/repo release-doctor.sh v1.2.3
#
# Env:
#   REPO   default LightHeart-Ventures/aish
#   AISH   path to the installed binary to version-check (default: `aish` on PATH)

set -uo pipefail

REPO="${REPO:-LightHeart-Ventures/aish}"
AISH_BIN="${AISH:-aish}"
EXPECTED_ASSETS=7   # 3 binaries + 3 .sha256 + SHA256SUMS

pass=0 warn=0 fail=0
ok()   { printf '  \033[32mPASS\033[0m  %s\n' "$1"; pass=$((pass+1)); }
wn()   { printf '  \033[33mWARN\033[0m  %s\n' "$1"; warn=$((warn+1)); }
bad()  { printf '  \033[31mFAIL\033[0m  %s\n' "$1"; fail=$((fail+1)); }
head() { printf '\n\033[1m%s\033[0m\n' "$1"; }

command -v gh  >/dev/null || { echo "release-doctor: gh not found on PATH" >&2; exit 2; }
command -v git >/dev/null || { echo "release-doctor: git not found on PATH" >&2; exit 2; }
command -v jq  >/dev/null || { echo "release-doctor: jq not found on PATH" >&2; exit 2; }

# ---- resolve the version under inspection -----------------------------------
VER="${1:-}"
if [ -z "$VER" ]; then
  VER="$(gh release list --repo "$REPO" --limit 1 --json tagName -q '.[0].tagName' 2>/dev/null)"
  [ -z "$VER" ] && { echo "release-doctor: no releases found on $REPO and no version arg given" >&2; exit 2; }
fi
case "$VER" in v*) : ;; *) VER="v$VER" ;; esac
VER_NUM="${VER#v}"

printf '\033[1maish release-doctor\033[0m — %s @ %s\n' "$VER" "$REPO"

# Make sure we have the tag + origin/main locally (best-effort; harmless if offline).
git fetch --tags --quiet origin 2>/dev/null || wn "git fetch failed (offline?) — using local refs"

# ---- 1. published release: exists? draft? assets? (§1) ----------------------
head "Published release (§1 — immutable / asset-less)"
REL_JSON="$(gh release view "$VER" --repo "$REPO" --json isDraft,assets,createdAt 2>/dev/null)"
if [ -z "$REL_JSON" ]; then
  wn "no release object for $VER yet — if you are PRE-tag this is expected; the workflow creates it. Never 'gh release create' before the tag push (§1)."
  REL_EXISTS=0
else
  REL_EXISTS=1
  IS_DRAFT="$(printf '%s' "$REL_JSON" | jq -r '.isDraft')"
  ASSET_N="$(printf '%s' "$REL_JSON" | jq -r '.assets | length')"
  ASSET_NAMES="$(printf '%s' "$REL_JSON" | jq -r '.assets[].name' | paste -sd, -)"
  if [ "$IS_DRAFT" = "true" ]; then
    wn "release is a DRAFT — mutable, workflow can still attach assets. OK pre-publish; must flip to published with assets when done."
  else
    ok "release is published (not draft)"
  fi
  if [ "$ASSET_N" -eq 0 ]; then
    bad "release has ZERO assets → :update is broken. Recover via §1 (bump forward, don't re-push a burned tag)."
  elif [ "$ASSET_N" -lt "$EXPECTED_ASSETS" ]; then
    wn "release has $ASSET_N/$EXPECTED_ASSETS assets [$ASSET_NAMES] — workflow may still be running or partially failed (§1)."
  else
    ok "release has $ASSET_N assets (expected $EXPECTED_ASSETS): $ASSET_NAMES"
  fi
fi

# ---- 2. Cargo.toml on the tagged commit (verify-version) --------------------
head "Tag ↔ Cargo.toml (verify-version)"
TAG_CARGO="$(git show "refs/tags/$VER:Cargo.toml" 2>/dev/null | grep -m1 '^version' | sed -E 's/.*"([^"]+)".*/\1/')"
if [ -z "$TAG_CARGO" ]; then
  wn "tag $VER not found locally (or Cargo.toml unreadable at tag) — cannot check tag↔Cargo. Fetch tags or pass a released version."
else
  if [ "$TAG_CARGO" = "$VER_NUM" ]; then
    ok "Cargo.toml @ $VER == $VER_NUM (tag and manifest agree)"
  else
    bad "Cargo.toml @ tag $VER says $TAG_CARGO, tag implies $VER_NUM — tag↔Cargo drift (§2). verify-version would have caught this only if it ran."
  fi
fi

# ---- 3. Cargo.toml on origin/main (bump merged? §2) -------------------------
head "origin/main ↔ tag (did the bump merge? §2)"
MAIN_CARGO="$(git show origin/main:Cargo.toml 2>/dev/null | grep -m1 '^version' | sed -E 's/.*"([^"]+)".*/\1/')"
if [ -z "$MAIN_CARGO" ]; then
  wn "cannot read origin/main:Cargo.toml — fetch origin first."
else
  if [ "$MAIN_CARGO" = "$VER_NUM" ]; then
    ok "origin/main Cargo.toml == $VER_NUM (bump landed on main)"
  else
    bad "origin/main Cargo.toml == $MAIN_CARGO but tag is $VER_NUM — bump was tagged off-main and never merged (§2). :update will drift forever."
  fi
fi

# ---- 4. Cargo.lock ↔ Cargo.toml on main (--locked gate, §4) -----------------
head "Cargo.lock sync on origin/main (--locked gate §4)"
LOCK_VER="$(git show origin/main:Cargo.lock 2>/dev/null \
  | awk '/^name = "aish"/{f=1} f&&/^version = /{gsub(/[",]/,"",$3); print $3; exit}')"
if [ -z "$LOCK_VER" ]; then
  wn "cannot read origin/main:Cargo.lock aish entry — fetch origin first."
elif [ -n "$MAIN_CARGO" ] && [ "$LOCK_VER" = "$MAIN_CARGO" ]; then
  ok "Cargo.lock aish == Cargo.toml ($LOCK_VER) on main — --locked will pass"
else
  bad "Cargo.lock aish=$LOCK_VER but Cargo.toml=$MAIN_CARGO on main — lockfile not refreshed; CI fails on --locked (§4). Run 'cargo build' and commit Cargo.lock."
fi

# ---- 5. installed binary self-report (§2) -----------------------------------
head "Installed binary self-report (tag↔Cargo drift §2)"
if command -v "$AISH_BIN" >/dev/null 2>&1; then
  BIN_VER="$("$AISH_BIN" --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
  if [ -z "$BIN_VER" ]; then
    wn "'$AISH_BIN --version' produced no semver — skipping."
  elif [ "$BIN_VER" = "$VER_NUM" ]; then
    ok "installed $AISH_BIN reports $BIN_VER == $VER_NUM"
  else
    wn "installed $AISH_BIN reports $BIN_VER, inspected release is $VER_NUM. If $BIN_VER has NO matching release, it's a phantom build off an unreleased bump (§2) — reconcile 'gh release list' before blaming :update."
  fi
else
  wn "'$AISH_BIN' not on PATH — skipping installed-binary check (set AISH=/path/to/aish to enable)."
fi

# ---- verdict ----------------------------------------------------------------
printf '\n\033[1mSummary:\033[0m %d pass, %d warn, %d fail\n' "$pass" "$warn" "$fail"
if [ "$fail" -gt 0 ]; then
  printf '\033[31m✗ release-doctor found hard problems — see FAIL lines and the cited SKILL.md sections.\033[0m\n'
  exit 1
fi
printf '\033[32m✓ no hard failures.\033[0m %s\n' "$([ "$warn" -gt 0 ] && echo 'Review WARN lines (may be expected pre-publish).' || echo 'Release looks consistent.')"
exit 0
