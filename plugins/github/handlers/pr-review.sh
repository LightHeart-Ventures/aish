#!/usr/bin/env bash
# SPR-069 / TASK-387 — GitHub pull_request handler.
#
# Contract (aish-webhook-client dispatcher, no shell in the loop — this script
# is fork/exec'd directly as argv[0]):
#   * stdin   : the raw GitHub `pull_request` event payload as JSON.
#   * env     : WEBHOOK_ID, WEBHOOK_TENANT_ID, WEBHOOK_PLUGIN_ID, WEBHOOK_EVENT_TYPE.
#   * stdout  : a single concise summary line (captured + audited by the dispatcher).
#   * exit 0  : success. Non-zero is logged as a handler failure but never blocks
#               sibling handlers (the dispatcher isolates every handler).
#
# JSON is parsed with python3 (ubiquitous, no jq dependency). The script is
# location-independent: it reads only stdin + env, so it works regardless of the
# process cwd the dispatcher runs it under.
#
# TASK-373 (SPR-104) — OPT-IN review agent on PR open. When
# GITHUB_PR_AUTOREVIEW=1 and the PR is opened / reopened / ready_for_review and
# NOT a draft, this handler detaches a background aish coordinator that reviews
# the PR (mirrors the workflow-run.sh fix-ci worker). It is:
#   * opt-in       — OFF unless GITHUB_PR_AUTOREVIEW=1 (each review costs tokens).
#   * non-blocking — setsid-detached (nohup where setsid is absent, e.g. macOS)
#                    with closed stdin; the handler returns immediately.
#   * idempotent   — a marker per (repo, PR, head SHA) dedupes webhook
#                    redeliveries and opened+ready_for_review on the same SHA.
#   * best-effort  — dispatch never changes this handler's exit status.
set -euo pipefail

payload="$(cat)"

# Scratch file python writes shell-safe KEY='value' fields to; bash sources it
# afterwards. Nothing from the payload is ever eval'd or spliced into a command.
# (Template must END in X's: BSD/macOS mktemp does not randomise a template
# with a suffix after the X's, so concurrent runs would collide.)
fields="$(mktemp "${TMPDIR:-/tmp}/gh-pr.XXXXXX")"
trap 'rm -f "$fields"' EXIT

rc=0
python3 - "$payload" "$fields" <<'PY' || rc=$?
import json, os, sys

raw = sys.argv[1] if len(sys.argv) > 1 else "{}"
fields_path = sys.argv[2] if len(sys.argv) > 2 else "/dev/null"
try:
    ev = json.loads(raw) if raw.strip() else {}
except json.JSONDecodeError as e:
    print(f"[github/pr] malformed payload: {e}", file=sys.stderr)
    sys.exit(2)

pr = ev.get("pull_request") or {}
action = ev.get("action", "?")
num = pr.get("number", "?")
title = (pr.get("title") or "").strip()
author = ((pr.get("user") or {}).get("login")) or "?"
base = ((pr.get("base") or {}).get("ref")) or "?"
head = ((pr.get("head") or {}).get("ref")) or "?"
sha = ((pr.get("head") or {}).get("sha")) or ""
url = pr.get("html_url") or ""
repo = ((ev.get("repository") or {}).get("full_name")) or "?"
draft = pr.get("draft", False)

tenant = os.environ.get("WEBHOOK_TENANT_ID", "-")
draft_tag = " [draft]" if draft else ""
print(
    f"[github/pr] {repo}#{num} {action}{draft_tag} by @{author} "
    f"({head}→{base}) tenant={tenant}: {title} {url}".rstrip()
)

def sq(v):
    return "'" + str(v).replace("'", "'\\''") + "'"

with open(fields_path, "w") as fh:
    fh.write(f"PR_REPO={sq(repo)}\n")
    fh.write(f"PR_NUM={sq(num)}\n")
    fh.write(f"PR_ACTION={sq(action)}\n")
    fh.write(f"PR_DRAFT={1 if draft else 0}\n")
    fh.write(f"PR_SHA={sq(sha)}\n")
    fh.write(f"PR_HEAD={sq(head)}\n")
    fh.write(f"PR_BASE={sq(base)}\n")
    fh.write(f"PR_URL={sq(url)}\n")
PY

# ---------------------------------------------------------------------------
# Opt-in review agent dispatch (best-effort; never changes the exit status).
# ---------------------------------------------------------------------------
if [ "$rc" = "0" ] && [ -s "$fields" ]; then
    # shellcheck disable=SC1090
    . "$fields"
fi

if [ "${GITHUB_PR_AUTOREVIEW:-0}" = "1" ] && [ "$rc" = "0" ]; then
    case "${PR_ACTION:-}" in
        opened | reopened | ready_for_review) want=1 ;;
        *) want=0 ;;
    esac
    if [ "$want" = "1" ] && [ "${PR_DRAFT:-0}" = "1" ]; then
        echo "[github/pr] auto-review skipped: PR is a draft" >&2
    elif [ "$want" = "1" ]; then
        if ! command -v aish >/dev/null 2>&1; then
            echo "[github/pr] auto-review skipped: 'aish' not on PATH" >&2
        else
            sha7="${PR_SHA:0:7}"
            key="${PR_REPO:-unknown}-${PR_NUM:-0}-${PR_SHA:-nosha}"
            key="$(printf '%s' "$key" | tr -c 'A-Za-z0-9._-' '_')"
            marker="${TMPDIR:-/tmp}/aish-pr-autoreview-${key}.marker"
            if [ -e "$marker" ]; then
                echo "[github/pr] auto-review already dispatched for ${PR_REPO}#${PR_NUM}@${sha7}" >&2
            else
                : >"$marker" 2>/dev/null || true
                run_id="pr-review-$(printf '%s' "${PR_NUM:-0}" | tr -c '0-9' '_')-$(printf '%s' "$sha7" | tr -c 'A-Za-z0-9' '_')-$(date +%s)"
                log="${TMPDIR:-/tmp}/${run_id}.log"
                task="Review GitHub pull request #${PR_NUM} in repo ${PR_REPO} \
(${PR_HEAD} -> ${PR_BASE}, head ${PR_SHA}, ${PR_URL}). Use the pr-review skill: read the \
diff with 'gh pr diff ${PR_NUM} --repo ${PR_REPO}', check correctness, security and tests, \
then post your findings as a single PR review comment with 'gh pr review ${PR_NUM} \
--repo ${PR_REPO} --comment'. Do NOT push, approve, or merge."

                # Detach so the reviewer outlives this short-lived handler and
                # never blocks the dispatcher timeout. macOS has no setsid(1).
                if command -v setsid >/dev/null 2>&1; then
                    setsid aish --coordinator --run-id "$run_id" -c "$task" \
                        >"$log" 2>&1 </dev/null &
                else
                    nohup aish --coordinator --run-id "$run_id" -c "$task" \
                        >"$log" 2>&1 </dev/null &
                fi
                disown 2>/dev/null || true
                echo "[github/pr] review agent dispatched: run-id=${run_id} log=${log}" >&2
            fi
        fi
    fi
fi

exit "$rc"
