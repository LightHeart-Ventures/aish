#!/bin/sh
# Post-deploy smoke test for the aish skill mirror (TASK-696).
#
# POSIX sh, no bash-isms. Exits NON-ZERO on the first failed assertion so it can
# gate a deploy step directly.
#
# Usage:
#   ./scripts/smoke.sh                                  # defaults to skills.aish.sh
#   MIRROR=https://d111111abcdef8.cloudfront.net ./scripts/smoke.sh
#   SKILL=acme/git-helper ./scripts/smoke.sh
#
# MIRROR may be the custom hostname or the raw CloudFront domain -- testing the
# latter is how you verify the distribution before cert/DNS propagation lands.

set -eu

MIRROR="${MIRROR:-https://skills.aish.sh}"
SKILL="${SKILL:-acme/git-helper}"
MISSING_SKILL="${MISSING_SKILL:-nonexistent-owner-zzz/nonexistent-skill-zzz}"

MIRROR="$(printf '%s' "$MIRROR" | sed 's#/*$##')"

FAILURES=0
HDR_FILE="$(mktemp)"
BODY_FILE="$(mktemp)"
# shellcheck disable=SC2064
trap "rm -f '$HDR_FILE' '$BODY_FILE'" EXIT INT TERM

pass() { printf 'ok   -- %s\n' "$1"; }
fail() {
  printf 'FAIL -- %s\n' "$1" >&2
  FAILURES=$((FAILURES + 1))
}

# fetch <url> -> populates $HDR_FILE / $BODY_FILE, echoes the HTTP status.
# Does not use curl -f here because we need to inspect 404s deliberately.
fetch() {
  curl -sS -o "$BODY_FILE" -D "$HDR_FILE" -w '%{http_code}' \
    --max-time 30 --retry 2 --retry-delay 2 "$1" 2>/dev/null || printf '000'
}

# header_value <name> -- case-insensitive lookup, trimmed, lowercased name.
header_value() {
  tr -d '\r' <"$HDR_FILE" |
    awk -v want="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" '
      {
        line = $0
        idx = index(line, ":")
        if (idx > 0) {
          name = substr(line, 1, idx - 1)
          val  = substr(line, idx + 1)
          gsub(/^[ \t]+|[ \t]+$/, "", val)
          if (tolower(name) == want) print val
        }
      }' | tail -n 1
}

printf '== aish skill-mirror smoke test ==\n'
printf 'mirror: %s\n\n' "$MIRROR"

# ---------------------------------------------------------------------------
# 1. The catalog: /index.json must be 200, application/json, with cache-control
# ---------------------------------------------------------------------------
printf '%s\n' '-- /index.json'
status="$(fetch "$MIRROR/index.json")"

if [ "$status" = "200" ]; then
  pass "index.json returned 200"
else
  fail "index.json returned '$status', expected 200"
fi

ct="$(header_value content-type)"
case "$(printf '%s' "$ct" | tr '[:upper:]' '[:lower:]')" in
*application/json*) pass "index.json content-type is application/json (got: $ct)" ;;
*) fail "index.json content-type was '$ct', expected application/json" ;;
esac

cc="$(header_value cache-control)"
if [ -n "$cc" ]; then
  pass "index.json cache-control present (got: $cc)"
else
  fail "index.json cache-control header is MISSING"
fi

# CRITICAL: see cloudfront.tf -- a 429 + this header is special-cased by
# is_vercel_challenge() in src/skill_provider.rs into a bot-challenge error.
# Our mirror must never emit it.
xvm="$(header_value x-vercel-mitigated)"
if [ -z "$xvm" ]; then
  pass "index.json does NOT carry x-vercel-mitigated"
else
  fail "index.json carried x-vercel-mitigated: '$xvm' -- this breaks the aish client"
fi

# ---------------------------------------------------------------------------
# 2. A raw object: 200, text/plain, cache-control w/ stale-while-revalidate
# ---------------------------------------------------------------------------
printf '\n-- /%s/raw\n' "$SKILL"
status="$(fetch "$MIRROR/$SKILL/raw")"

if [ "$status" = "200" ]; then
  pass "$SKILL/raw returned 200"
else
  fail "$SKILL/raw returned '$status', expected 200"
fi

ct="$(header_value content-type)"
case "$(printf '%s' "$ct" | tr '[:upper:]' '[:lower:]')" in
*text/plain*) pass "raw content-type is text/plain (got: $ct)" ;;
*) fail "raw content-type was '$ct', expected text/plain" ;;
esac

cc="$(header_value cache-control)"
if [ -n "$cc" ]; then
  pass "raw cache-control present (got: $cc)"
else
  fail "raw cache-control header is MISSING"
fi

case "$cc" in
*stale-while-revalidate*) pass "raw cache-control carries stale-while-revalidate" ;;
*) fail "raw cache-control lacks stale-while-revalidate (got: '$cc')" ;;
esac

if [ -s "$BODY_FILE" ]; then
  pass "raw body is non-empty"
else
  fail "raw body was empty"
fi

xvm="$(header_value x-vercel-mitigated)"
if [ -z "$xvm" ]; then
  pass "raw does NOT carry x-vercel-mitigated"
else
  fail "raw carried x-vercel-mitigated: '$xvm' -- this breaks the aish client"
fi

# ---------------------------------------------------------------------------
# 3. ?version=<v> must serve the same object, NOT 404 (v1 ignores the query)
# ---------------------------------------------------------------------------
printf '\n-- /%s/raw?version=1.2.3 (query string must be ignored, not 404)\n' "$SKILL"
status="$(fetch "$MIRROR/$SKILL/raw?version=1.2.3")"

if [ "$status" = "200" ]; then
  pass "versioned raw URL returned 200 (query string ignored as designed)"
else
  fail "versioned raw URL returned '$status', expected 200 -- check query_strings_config"
fi

# ---------------------------------------------------------------------------
# 4. Negative: an unknown skill must 404 on raw.
#    (Only SEARCH must avoid 404 on empty -- that is TASK-697's concern.)
# ---------------------------------------------------------------------------
printf '\n-- /%s/raw (unknown skill, expect 404)\n' "$MISSING_SKILL"
status="$(fetch "$MIRROR/$MISSING_SKILL/raw")"

if [ "$status" = "404" ]; then
  pass "unknown skill returned 404"
else
  fail "unknown skill returned '$status', expected 404"
fi

xvm="$(header_value x-vercel-mitigated)"
if [ -z "$xvm" ]; then
  pass "404 response does NOT carry x-vercel-mitigated"
else
  fail "404 response carried x-vercel-mitigated: '$xvm'"
fi

# ---------------------------------------------------------------------------
# 5. https is mandatory: skill_provider::check_url refuses non-https origins.
# ---------------------------------------------------------------------------
case "$MIRROR" in
https://*) pass "mirror base URL is https (required by skill_provider::check_url)" ;;
*) fail "mirror base URL is not https: '$MIRROR'" ;;
esac

# ---------------------------------------------------------------------------
printf '\n== summary ==\n'
if [ "$FAILURES" -eq 0 ]; then
  printf 'all assertions passed\n'
  exit 0
fi

printf '%s assertion(s) FAILED\n' "$FAILURES" >&2
exit 1
