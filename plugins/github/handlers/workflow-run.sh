#!/usr/bin/env bash
# SPR-069 / TASK-387 — GitHub workflow_run handler (+ CI auto-fix worker).
#
# Fired on `workflow_run` completed events. Surfaces CI outcome, and returns a
# non-zero exit ONLY as an informational signal on failure/timeout conclusions
# (the dispatcher logs+audits it; sibling handlers are unaffected). stdin = raw
# GitHub `workflow_run` payload JSON; stdout = one summary line.
#
# NEW: when a PR's CI concludes in a bad state, this handler detaches a
# background aish coordinator ("a worker") that runs the `fix-ci` skill against
# the failed run to diagnose + push a fix on the PR branch. It is:
#   * opt-out         — set GITHUB_CI_AUTOFIX=0 to disable (default: enabled).
#   * PR-scoped       — only fires when the event carries an associated PR (or,
#                       failing that, a head branch to work on).
#   * non-blocking    — the worker is setsid-detached so the handler returns
#                       well inside its dispatcher timeout.
#   * idempotent      — a per-run marker file dedupes webhook redeliveries so a
#                       single failed run never spawns duplicate workers.
set -euo pipefail

payload="$(cat)"

# Deterministic scratch path python writes machine-readable fields to; bash
# sources it after to decide whether to spawn the auto-fix worker.
fields="$(mktemp "${TMPDIR:-/tmp}/gh-ci-XXXXXX.env")"
trap 'rm -f "$fields"' EXIT

rc=0
python3 - "$payload" "$fields" <<'PY' || rc=$?
import json, os, sys

raw = sys.argv[1] if len(sys.argv) > 1 else "{}"
fields_path = sys.argv[2] if len(sys.argv) > 2 else "/dev/null"
try:
    ev = json.loads(raw) if raw.strip() else {}
except json.JSONDecodeError as e:
    print(f"[github/ci] malformed payload: {e}", file=sys.stderr)
    sys.exit(2)

wr = ev.get("workflow_run") or {}
name = wr.get("name") or "?"
status = wr.get("status") or "?"
conclusion = wr.get("conclusion") or "?"
branch = wr.get("head_branch") or "?"
run_num = wr.get("run_number", "?")
run_id = wr.get("id") or ""
url = wr.get("html_url") or ""
repo = ((ev.get("repository") or {}).get("full_name")) or "?"
tenant = os.environ.get("WEBHOOK_TENANT_ID", "-")

# Associated PR (workflow_run.pull_requests is populated for same-repo PRs).
prs = wr.get("pull_requests") or []
pr_num = ""
if prs and isinstance(prs, list):
    pr_num = str((prs[0] or {}).get("number") or "")

bad = {"failure", "timed_out", "cancelled", "startup_failure"}
is_bad = conclusion in bad
mark = "\u2717" if is_bad else ("\u2713" if conclusion == "success" else "\u2022")
print(
    f"[github/ci] {mark} {repo} '{name}' run#{run_num} ({branch}) "
    f"{status}/{conclusion} tenant={tenant} {url}".rstrip()
)

# Emit shell-safe KEY=VALUE fields for the bash wrapper (single-quoted, with
# any embedded single-quotes escaped) so nothing from the payload is eval'd.
def sq(v):
    return "'" + str(v).replace("'", "'\\''") + "'"

with open(fields_path, "w") as fh:
    fh.write(f"CI_BAD={1 if is_bad else 0}\n")
    fh.write(f"CI_REPO={sq(repo)}\n")
    fh.write(f"CI_BRANCH={sq(branch)}\n")
    fh.write(f"CI_RUN_ID={sq(run_id)}\n")
    fh.write(f"CI_RUN_NUM={sq(run_num)}\n")
    fh.write(f"CI_WORKFLOW={sq(name)}\n")
    fh.write(f"CI_URL={sq(url)}\n")
    fh.write(f"CI_PR={sq(pr_num)}\n")

# Informational non-zero on a bad CI conclusion so downstream audit can key on it.
sys.exit(1 if is_bad else 0)
PY

# ---------------------------------------------------------------------------
# CI auto-fix worker dispatch (best-effort; never changes the handler's exit
# status, which must keep signalling the CI conclusion to the dispatcher audit).
# ---------------------------------------------------------------------------
if [ -f "$fields" ]; then
    # shellcheck disable=SC1090
    . "$fields"
fi

autofix="${GITHUB_CI_AUTOFIX:-1}"
if [ "${CI_BAD:-0}" = "1" ] && [ "$autofix" != "0" ]; then
    if ! command -v aish >/dev/null 2>&1; then
        echo "[github/ci] auto-fix skipped: 'aish' not on PATH" >&2
    elif [ -z "${CI_PR:-}" ] && { [ -z "${CI_BRANCH:-}" ] || [ "${CI_BRANCH}" = "?" ]; }; then
        echo "[github/ci] auto-fix skipped: no PR / branch to work on" >&2
    else
        # Dedupe redeliveries of the same failed run.
        marker="${TMPDIR:-/tmp}/aish-ci-autofix-${CI_RUN_ID:-unknown}.marker"
        if [ -e "$marker" ]; then
            echo "[github/ci] auto-fix already dispatched for run ${CI_RUN_ID}" >&2
        else
            : > "$marker" 2>/dev/null || true
            run_id="ci-autofix-${CI_RUN_ID:-0}-$(date +%s)"
            log="${TMPDIR:-/tmp}/${run_id}.log"
            pr_clause=""
            [ -n "${CI_PR:-}" ] && pr_clause="PR #${CI_PR} "
            task="GitHub CI failed for ${pr_clause}in repo ${CI_REPO} on branch '${CI_BRANCH}' \
(workflow '${CI_WORKFLOW}' run ${CI_RUN_ID}, ${CI_URL}). \
Use the fix-ci skill: check out ${CI_REPO} at branch '${CI_BRANCH}', inspect the failed run with \
'gh run view ${CI_RUN_ID} --log-failed', find the root cause, implement the smallest correct fix on \
that branch, run the project's test/lint gate to confirm green, then commit and push to the PR branch. \
Do NOT push to the default branch. Report the fix and the resulting commit/PR."

            # Detach: setsid + background + closed stdin so the worker outlives
            # this short-lived handler and never blocks the dispatcher timeout.
            setsid aish --coordinator --run-id "$run_id" -c "$task" \
                >"$log" 2>&1 </dev/null &
            disown 2>/dev/null || true
            echo "[github/ci] auto-fix worker dispatched: run-id=${run_id} log=${log}" >&2
        fi
    fi
fi

exit "$rc"
