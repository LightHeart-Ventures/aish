#!/usr/bin/env bash
# hello-world login handler (Phase 0.5.5 demo).
#
# `aish login hello-world` routes here because plugin.json declares
#   "provides": { "login": "hello-world" }
#
# CONTRACT
#   * aish runs this script via fork/exec (no shell) with these env vars set:
#       AISH_PLUGIN_ID        the plugin id  (hello-world)
#       AISH_LOGIN_NAME       the login name (hello-world) — the credential profile suffix
#       AISH_TENANT_ID        tenant id when known (may be empty)
#       AISH_CREDENTIALS_FILE where aish will persist the profile (~/.aish/credentials)
#   * STDIN and STDERR are INHERITED — print prompts / device-code URLs / status
#     to STDERR, and read interactive input from STDIN if you need it.
#   * On SUCCESS: print a single flat JSON object of credential fields to STDOUT
#     and exit 0. aish captures STDOUT and writes it to
#     [profile:$AISH_LOGIN_NAME] in the credentials file (mode 0600).
#   * On FAILURE: write a message to STDERR and exit non-zero. Nothing is stored.
#
# This demo simulates a device-code flow without contacting any server: it shows
# a fake verification URL + user code on STDERR, then emits a stub token on
# STDOUT. Real plugins swap the sleep for a genuine device-code poll / OAuth
# browser round-trip.
set -euo pipefail

# --- 1. Show the user what to do (STDERR keeps STDOUT clean for JSON) ---------
user_code="WXYZ-1234"
verify_url="https://example.com/device"
echo "hello-world login (demo device-code flow)" >&2
echo "  1. Visit: ${verify_url}" >&2
echo "  2. Enter code: ${user_code}" >&2
echo "  (tenant='${AISH_TENANT_ID:-none}', plugin='${AISH_PLUGIN_ID:-?}')" >&2

# --- 2. "Poll" for authorization (instant in the demo) ------------------------
# A real handler would loop here hitting the token endpoint until the user
# approves, honoring the interval + expiry the device-code response gave it.
sleep 0

# --- 3. Emit credentials as flat JSON on STDOUT -------------------------------
# Values must be scalars (string/number/bool); nested objects/arrays are
# rejected. expires_at is an ISO-8601 timestamp; expires_in is seconds.
expires_at="$(date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u +%Y-%m-%dT%H:%M:%SZ)"
cat <<JSON
{
  "access_token": "demo-access-token-$RANDOM",
  "refresh_token": "demo-refresh-token-$RANDOM",
  "token_type": "Bearer",
  "expires_in": 3600,
  "expires_at": "${expires_at}"
}
JSON
