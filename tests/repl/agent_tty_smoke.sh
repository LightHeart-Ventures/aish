#!/usr/bin/env bash
#
# agent_tty_smoke.sh — drive the real aish REPL through coder/agent-tty and
# assert on observable terminal state.
#
#   https://github.com/coder/agent-tty
#
# WHY agent-tty (not rexpect / tmux / bare PTY):
#   aish is a full-screen alt-screen TUI REPL (ratatui: banner + bottom status
#   bar + input line), not a line-oriented POSIX shell. agent-tty gives us an
#   ISOLATED terminal host (`--home`), a real PTY, a semantic screen renderer
#   (libghostty-vt) with `wait --text` / `--screen-stable-ms` observability, and
#   machine-readable `--json` envelopes with STABLE exit codes — so we can gate
#   on rendered state instead of blind sleeps and scrape structured output
#   instead of raw bytes. `tests/pty_harness.rs` covers the kernel job-control
#   invariants at the syscall level; THIS harness covers the end-user REPL as it
#   actually renders on a terminal.
#
# HERMETIC: uses only aish built-in `:commands` (`:help`, `:quit`). It never
#   sends an intent line, so it makes NO model/API call and needs NO
#   ANTHROPIC_API_KEY. Safe to run offline / in CI.
#
# IMPORTANT: aish is NOT bash, so we drive it with agent-tty `type` +
#   `sendKeys ["Enter"]` (literal interactive typing) — NEVER `run`, whose
#   hidden shell completion-marker assumes a POSIX shell and pollutes the TUI.
#
# Usage:
#   tests/repl/agent_tty_smoke.sh
#
# Env knobs:
#   AISH_BIN            path to the aish binary (else: ./target/release/aish,
#                       ./target/debug/aish, then `command -v aish`).
#   AGENT_TTY_VERSION   npm version to run via npx (default: 0.5.0).
#   ARTIFACT_DIR        where to drop the asciicast/snapshot (default:
#                       tests/repl/artifacts).
#   AISH_REPL_STRICT=1  turn "prerequisite missing" SKIPs into hard failures
#                       (CI sets this once Node 24 + a built binary are present).
#   DEBUG=1             xtrace every agent-tty call.
#
# Exit: 0 = pass (or skipped when a prerequisite is absent and not strict),
#       nonzero = at least one assertion failed.

set -euo pipefail

# ── config ──────────────────────────────────────────────────────────────────
AGENT_TTY_VERSION="${AGENT_TTY_VERSION:-0.5.0}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ARTIFACT_DIR="${ARTIFACT_DIR:-$REPO_ROOT/tests/repl/artifacts}"
STRICT="${AISH_REPL_STRICT:-0}"
COLS=100
ROWS=30

pass_count=0
fail_count=0

say()  { printf '  %s\n' "$*"; }
ok()   { printf '  \033[32mPASS\033[0m %s\n' "$*"; pass_count=$((pass_count+1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$*"; fail_count=$((fail_count+1)); }
skip() {
  printf '  \033[33mSKIP\033[0m %s\n' "$*"
  if [ "$STRICT" = "1" ]; then
    printf '  (AISH_REPL_STRICT=1 → treating SKIP as failure)\n'
    exit 1
  fi
  exit 0
}

# ── resolve prerequisites ───────────────────────────────────────────────────
command -v jq   >/dev/null 2>&1 || skip "jq not found (needed to parse --json envelopes)"
command -v node >/dev/null 2>&1 || skip "node not found (agent-tty needs Node >=24 <27)"

resolve_aish() {
  if [ -n "${AISH_BIN:-}" ] && [ -x "$AISH_BIN" ]; then echo "$AISH_BIN"; return; fi
  for c in "$REPO_ROOT/target/release/aish" "$REPO_ROOT/target/debug/aish"; do
    [ -x "$c" ] && { echo "$c"; return; }
  done
  command -v aish 2>/dev/null || true
}
AISH_BIN="$(resolve_aish)"
[ -n "$AISH_BIN" ] && [ -x "$AISH_BIN" ] || skip "no aish binary found (set AISH_BIN, or run 'make build-fast')"

# agent-tty runner: prefer an installed binary, else npx a pinned version.
if command -v agent-tty >/dev/null 2>&1; then
  ATTY=(agent-tty)
else
  command -v npx >/dev/null 2>&1 || skip "neither agent-tty nor npx on PATH"
  ATTY=(npx --yes "agent-tty@${AGENT_TTY_VERSION}")
fi

# ── isolated home + artifacts + cleanup ─────────────────────────────────────
AGENT_HOME="$(mktemp -d)"
mkdir -p "$ARTIFACT_DIR"
SID=""

AT() {
  [ "${DEBUG:-0}" = "1" ] && set -x
  "${ATTY[@]}" --home "$AGENT_HOME" "$@"
  local rc=$?
  { set +x; } 2>/dev/null
  return $rc
}

cleanup() {
  if [ -n "$SID" ]; then AT destroy "$SID" --json >/dev/null 2>&1 || true; fi
  rm -rf "$AGENT_HOME" 2>/dev/null || true
}
trap cleanup EXIT

echo "aish REPL smoke via agent-tty"
say "aish       : $AISH_BIN"
say "agent-tty  : ${ATTY[*]}"
say "home       : $AGENT_HOME"
say "artifacts  : $ARTIFACT_DIR"
echo

# ── preflight: doctor ───────────────────────────────────────────────────────
DOC="$(AT doctor --json 2>/dev/null || true)"
for cap in snapshot wait; do
  st="$(printf '%s' "$DOC" | jq -r --arg c "$cap" '.result.capabilities[]? | select(.name==$c) | .status' 2>/dev/null || true)"
  if [ "$st" != "available" ]; then
    skip "agent-tty capability '$cap' is '$st' (need 'available'); doctor could not confirm the semantic renderer"
  fi
done
ok "doctor: snapshot + wait capabilities available"

# ── helpers ─────────────────────────────────────────────────────────────────
snapshot_text() { AT snapshot "$SID" --format text --json | jq -r '.result.text'; }

assert_contains() { # <haystack> <needle> <label>
  if printf '%s' "$1" | grep -qF -- "$2"; then ok "$3"; else
    bad "$3 — expected to find: $2"
    printf '%s\n' "$1" | tail -8 | sed 's/^/      | /'
  fi
}

# ── 1. boot ─────────────────────────────────────────────────────────────────
SID="$(AT create --json --cols "$COLS" --rows "$ROWS" -- "$AISH_BIN" | jq -r '.result.sessionId')"
[ -n "$SID" ] && [ "$SID" != "null" ] || { bad "create returned no sessionId"; exit 1; }
say "session    : $SID"

booted="$(AT wait "$SID" --text 'AI-native shell' --timeout 20000 --json | jq -r '.result.matched')"
[ "$booted" = "true" ] && ok "REPL booted (banner 'AI-native shell' rendered)" \
                       || bad "REPL did not render boot banner within 20s"

boot_snap="$(snapshot_text)"
assert_contains "$boot_snap" 'aish v'  "banner shows version string"
assert_contains "$boot_snap" '❯'       "input prompt glyph rendered"

# ── 2. built-in :help renders (hermetic — no model call) ────────────────────
AT batch "$SID" \
  '[{"type":":help"},{"sendKeys":["Enter"]},{"wait":{"screenStableMs":1200,"timeout":15000}}]' \
  --json >/tmp/aish_help_batch.$$ 2>&1 || true
completed="$(jq -r '.result.completedCount // 0' /tmp/aish_help_batch.$$ 2>/dev/null || echo 0)"
rm -f /tmp/aish_help_batch.$$
[ "$completed" = "3" ] && ok ":help batch applied (3/3 steps)" \
                       || bad ":help batch did not complete (completed=$completed)"

help_snap="$(snapshot_text)"
assert_contains "$help_snap" ':quit'  ":help output lists :quit"
assert_contains "$help_snap" 'Ctrl-O' ":help output lists the Ctrl-O binding"

# ── 3. artifact: asciicast recording (reviewer-facing proof) ────────────────
if AT record export "$SID" --format asciicast --out "$ARTIFACT_DIR/aish_repl_smoke.cast" --json >/dev/null 2>&1; then
  ok "asciicast exported → $ARTIFACT_DIR/aish_repl_smoke.cast"
else
  say "(asciicast export unavailable — skipped, non-fatal)"
fi
# Best-effort screenshot; needs Playwright chromium (npx playwright install).
if AT screenshot "$SID" --out "$ARTIFACT_DIR/aish_repl_smoke.png" --json >/dev/null 2>&1; then
  ok "screenshot exported → $ARTIFACT_DIR/aish_repl_smoke.png"
else
  say "(screenshot unavailable — no Playwright chromium; run 'npx playwright install chromium' to enable)"
fi

# ── 4. clean exit via :quit ─────────────────────────────────────────────────
# `wait --exit` is a standalone verb (not a batch step), so type the command
# first, then wait for the process to actually terminate.
AT batch "$SID" '[{"type":":quit"},{"sendKeys":["Enter"]}]' --json >/dev/null 2>&1 || true
timed_out="$(AT wait "$SID" --exit --timeout 10000 --json | jq -r '.result.timedOut')"
if [ "$timed_out" = "false" ]; then
  term="$(AT inspect "$SID" --json 2>/dev/null | jq -r '.result.terminationCategory // "exited"' 2>/dev/null || echo exited)"
  ok ":quit terminated the REPL cleanly (${term})"
else
  bad ":quit did not exit the process within 10s"
fi

# ── summary ─────────────────────────────────────────────────────────────────
echo
echo "── summary: ${pass_count} passed, ${fail_count} failed ──"
[ "$fail_count" -eq 0 ] || exit 1
