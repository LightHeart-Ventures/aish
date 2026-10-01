#!/bin/sh
#
# guardrail-smoke.sh — verify TASK-700's cost guardrails against a deployed
# skill mirror.
#
# POSIX sh. No bash-isms. Exits non-zero on the first failed assertion unless
# --keep-going is passed, in which case it runs everything and exits non-zero
# if anything failed.
#
# Implements the test plan from TASK-700's engineering spec:
#   1. cache-control present on all three path classes
#   2. >60 req/min from one IP against search => HTTP 429 + retry-after,
#      and NOT x-vercel-mitigated
#   3. limit=10000 returns at most 100 rows
#   4. fire a synthetic budget/egress alert end to end
#
# WHICH CHECKS NEED WHAT
#   check 1  requires a DEPLOYED stack (HTTP only)
#   check 2  requires a DEPLOYED stack, sends real traffic, opt-in
#   check 3  requires a DEPLOYED stack (HTTP only)
#   check 4  requires a DEPLOYED stack AND AWS credentials, opt-in
#
# Nothing here can run before TASK-696's hosting module is applied.
#
set -eu

MIRROR_BASE="${MIRROR_BASE:-https://skills.aish.sh}"
SAMPLE_SKILL="${SAMPLE_SKILL:-}"
ALERT_TOPIC_ARN="${ALERT_TOPIC_ARN:-}"
RATE_LIMIT_REQUESTS="${RATE_LIMIT_REQUESTS:-400}"

include_rate_limit=0
fire_synthetic_alert=0
keep_going=0
failures=0

usage() {
	cat <<'EOF'
Usage: sh guardrail-smoke.sh [options]

Options:
  --include-rate-limit     Run check 2. Sends several hundred requests from this
                           host's IP at the search endpoint, deliberately
                           tripping the WAF rate-based rule. Your IP will be
                           blocked on that path for the remainder of the
                           5-minute evaluation window. Do not run this from a
                           shared/NAT'd office IP during working hours.
  --fire-synthetic-alert   Run check 4. Publishes a synthetic message to the
                           guardrail SNS topic to prove the notification path
                           works. Requires AWS credentials and either
                           ALERT_TOPIC_ARN or a readable terraform output.
  --keep-going             Run every selected check even after a failure.
  -h, --help               Show this help.

Environment:
  MIRROR_BASE           Base URL under test. Default https://skills.aish.sh
  SAMPLE_SKILL          "owner/name" of a skill known to exist in the catalog.
                        If unset, check 1 discovers one from /index.json.
  ALERT_TOPIC_ARN       SNS topic ARN for check 4. If unset, read from
                        `terraform output -raw guardrails_alert_topic_arn`.
  RATE_LIMIT_REQUESTS   Requests check 2 issues. Default 400 (the default WAF
                        search limit is 300 per 5-minute window).

Exit status: 0 if every selected check passed, non-zero otherwise.
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--include-rate-limit) include_rate_limit=1 ;;
	--fire-synthetic-alert) fire_synthetic_alert=1 ;;
	--keep-going) keep_going=1 ;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		printf 'unknown option: %s\n\n' "$1" >&2
		usage >&2
		exit 2
		;;
	esac
	shift
done

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

pass() { printf '  PASS  %s\n' "$1"; }

fail() {
	printf '  FAIL  %s\n' "$1" >&2
	failures=$((failures + 1))
	if [ "$keep_going" -eq 0 ]; then
		printf '\n%s\n' "aborting on first failure (pass --keep-going to continue)" >&2
		exit 1
	fi
}

info() { printf '        %s\n' "$1"; }

section() { printf '\n== %s\n' "$1"; }

need() {
	if ! command -v "$1" >/dev/null 2>&1; then
		printf 'required command not found: %s\n' "$1" >&2
		exit 2
	fi
}

# Fetch response headers only, lowercased, into a temp file.
# Usage: head_to <url> <outfile>
head_to() {
	curl -sS -D - -o /dev/null --max-time 20 "$1" 2>/dev/null |
		tr 'A-Z' 'a-z' >"$2"
}

# Count JSON array elements. Uses jq when available; otherwise counts
# occurrences of the mandatory "reference" field, which every SearchResult row
# carries exactly once.
count_rows() {
	_file="$1"
	if command -v jq >/dev/null 2>&1; then
		jq 'length' <"$_file"
	else
		tr ',' '\n' <"$_file" | grep -c '"reference"' || true
	fi
}

need curl

printf 'guardrail smoke test\n'
printf 'target: %s\n' "$MIRROR_BASE"

# ---------------------------------------------------------------------------
# CHECK 1 — cache-control on all three path classes.  REQUIRES DEPLOYED STACK.
#
# The cache is the primary cost control (see GUARDRAILS.md "Layering
# rationale"), so a missing cache-control header is the single highest-impact
# regression this script can catch: the stack would still work, and would
# quietly cost orders of magnitude more.
# ---------------------------------------------------------------------------
section "check 1: cache-control headers [requires deployed stack]"

hdr=$(mktemp)
body=$(mktemp)
# shellcheck disable=SC2064
trap "rm -f '$hdr' '$body'" EXIT INT TERM

# --- /index.json ---
if ! curl -fsS --max-time 20 -o "$body" "$MIRROR_BASE/index.json"; then
	fail "GET /index.json did not return success"
else
	head_to "$MIRROR_BASE/index.json" "$hdr"
	if grep -q '^cache-control:' "$hdr"; then
		pass "/index.json carries cache-control"
		info "$(grep '^cache-control:' "$hdr" | head -n 1)"
		if grep '^cache-control:' "$hdr" | grep -q 's-maxage=300'; then
			pass "/index.json cache-control pins s-maxage=300"
		else
			fail "/index.json cache-control is missing the expected s-maxage=300"
		fi
	else
		fail "/index.json is missing cache-control"
	fi
fi

# Discover a real skill reference for the raw-path check if one was not given.
if [ -z "$SAMPLE_SKILL" ] && [ -s "$body" ]; then
	if command -v jq >/dev/null 2>&1; then
		SAMPLE_SKILL=$(jq -r '.[0].reference // empty' <"$body" 2>/dev/null || true)
	else
		SAMPLE_SKILL=$(
			tr ',' '\n' <"$body" |
				grep '"reference"' |
				head -n 1 |
				sed -e 's/.*"reference"[[:space:]]*:[[:space:]]*"//' -e 's/".*//'
		)
	fi
	[ -n "$SAMPLE_SKILL" ] && info "discovered sample skill: $SAMPLE_SKILL"
fi

# --- /{owner}/{name}/raw ---
if [ -z "$SAMPLE_SKILL" ]; then
	fail "could not determine a sample skill; set SAMPLE_SKILL=owner/name"
else
	raw_url="$MIRROR_BASE/$SAMPLE_SKILL/raw"
	head_to "$raw_url" "$hdr"
	if grep -q '^cache-control:' "$hdr"; then
		pass "$SAMPLE_SKILL/raw carries cache-control"
		info "$(grep '^cache-control:' "$hdr" | head -n 1)"
		if grep '^cache-control:' "$hdr" | grep -q 's-maxage=86400'; then
			pass "raw cache-control pins s-maxage=86400"
		else
			fail "raw cache-control is missing the expected s-maxage=86400"
		fi
		if grep '^cache-control:' "$hdr" | grep -q 'stale-while-revalidate=604800'; then
			pass "raw cache-control pins stale-while-revalidate=604800"
		else
			fail "raw cache-control is missing stale-while-revalidate=604800"
		fi
	else
		fail "$SAMPLE_SKILL/raw is missing cache-control"
	fi
fi

# --- /api/v1/search ---
head_to "$MIRROR_BASE/api/v1/search?q=test&limit=5" "$hdr"
if grep -q '^cache-control:' "$hdr"; then
	pass "/api/v1/search carries cache-control"
	info "$(grep '^cache-control:' "$hdr" | head -n 1)"
	if grep '^cache-control:' "$hdr" | grep -q 'max-age=60'; then
		pass "search cache-control pins max-age=60"
	else
		fail "search cache-control is missing the expected max-age=60"
	fi
else
	fail "/api/v1/search is missing cache-control"
fi

# No path class may ever carry the Vercel mitigation header, 429 or not.
for probe in "$MIRROR_BASE/index.json" "$MIRROR_BASE/api/v1/search?q=test"; do
	head_to "$probe" "$hdr"
	if grep -q '^x-vercel-mitigated:' "$hdr"; then
		fail "$probe carries x-vercel-mitigated (see GUARDRAILS.md)"
	else
		pass "$probe does not carry x-vercel-mitigated"
	fi
done

# ---------------------------------------------------------------------------
# CHECK 3 — server-side limit clamp.  REQUIRES DEPLOYED STACK.
#
# Run before check 2, because check 2 intentionally gets this IP blocked on the
# search path for the rest of the evaluation window.
# ---------------------------------------------------------------------------
section "check 3: limit clamp [requires deployed stack]"

if curl -fsS --max-time 20 -o "$body" "$MIRROR_BASE/api/v1/search?q=&limit=10000"; then
	rows=$(count_rows "$body")
	if [ -z "$rows" ]; then
		fail "could not count rows in the search response"
	elif [ "$rows" -le 100 ]; then
		pass "limit=10000 returned $rows rows (<= 100)"
	else
		fail "limit=10000 returned $rows rows; server-side clamp to 100 is not in effect"
	fi
else
	fail "GET /api/v1/search?limit=10000 did not return success"
fi

# ---------------------------------------------------------------------------
# CHECK 2 — per-IP rate limit.  REQUIRES DEPLOYED STACK. OPT-IN, SENDS TRAFFIC.
#
# Deliberately last of the HTTP checks: it ends with this host's IP blocked on
# the search path until the WAF evaluation window rolls over (~5 min).
#
# The decisive assertion is not merely "we got a 429" — it is that the 429 is a
# PLAIN one. `is_vercel_challenge` in src/skill_provider.rs matches 429 AND
# `x-vercel-mitigated: challenge` together and rewrites that pairing into
# aish's bot-challenge error, which would tell a throttled user the mirror is
# blocked by a bot challenge and advise them to switch to the mirror they are
# already using. See GUARDRAILS.md.
# ---------------------------------------------------------------------------
section "check 2: per-IP rate limit [requires deployed stack; opt-in]"

if [ "$include_rate_limit" -eq 0 ]; then
	info "SKIPPED — pass --include-rate-limit to run (sends real traffic)"
else
	info "issuing $RATE_LIMIT_REQUESTS requests at $MIRROR_BASE/api/v1/search"
	info "this host's IP will be rate limited on that path for ~5 minutes"

	got_429=0
	i=0
	while [ "$i" -lt "$RATE_LIMIT_REQUESTS" ]; do
		i=$((i + 1))
		code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
			"$MIRROR_BASE/api/v1/search?q=ratelimitprobe$i" || echo 000)
		if [ "$code" = "429" ]; then
			got_429=1
			info "first 429 after $i requests"
			break
		fi
	done

	if [ "$got_429" -eq 0 ]; then
		fail "no 429 after $RATE_LIMIT_REQUESTS requests; the rate limit is not in effect"
	else
		pass "search endpoint returned 429 under sustained load"

		head_to "$MIRROR_BASE/api/v1/search?q=ratelimitprobe-headers" "$hdr"

		if grep -q '^retry-after:' "$hdr"; then
			pass "429 carries retry-after"
			info "$(grep '^retry-after:' "$hdr" | head -n 1)"
		else
			fail "429 is missing retry-after"
		fi

		if grep -q '^x-vercel-mitigated:' "$hdr"; then
			fail "429 carries x-vercel-mitigated — this masquerades as skill.fish's Vercel challenge in the aish client"
		else
			pass "429 does NOT carry x-vercel-mitigated"
		fi

		if grep -q '^http/.* 429' "$hdr" || grep -q '429' "$hdr"; then
			pass "status line confirms 429"
		fi
	fi
fi

# ---------------------------------------------------------------------------
# CHECK 4 — synthetic budget/egress alert.
# REQUIRES DEPLOYED STACK AND AWS CREDENTIALS. OPT-IN.
#
# WHY THIS CHECK EXISTS
#   An SNS email subscription is created in `pending confirmation` state. AWS
#   emails the subscriber a link that a HUMAN must click. Until that happens
#   the CloudWatch alarm fires into the void, and the stack looks perfectly
#   healthy while being unable to tell anyone about a problem. This is the most
#   commonly skipped step in a cost-alarm setup.
#
#   Proving the alert path BEFORE it is needed is the entire point.
#
# MANUAL PROCEDURE (equivalent, no credentials in this script's path)
#
#   a) Confirm the subscription is live, not pending:
#        aws sns list-subscriptions-by-topic --topic-arn "$ALERT_TOPIC_ARN" \
#          --query 'Subscriptions[].[Endpoint,SubscriptionArn]' --output table
#      A SubscriptionArn of the literal string "PendingConfirmation" means the
#      recipient has not clicked the link. Resend with:
#        aws sns subscribe --topic-arn "$ALERT_TOPIC_ARN" \
#          --protocol email --notification-endpoint gregory@hohertz.com
#
#   b) Fire a synthetic alert (what --fire-synthetic-alert automates):
#        aws sns publish --topic-arn "$ALERT_TOPIC_ARN" \
#          --subject 'aish skill mirror: SYNTHETIC guardrail test' \
#          --message 'Synthetic alert. No action required.'
#      Confirm arrival in the recipient's inbox.
#
#   c) Exercise the CloudWatch alarm path itself, which also proves the alarm's
#      action wiring and not just the topic:
#        aws cloudwatch set-alarm-state \
#          --alarm-name "<prefix>-bytes-downloaded" \
#          --state-value ALARM \
#          --state-reason 'synthetic guardrail test' \
#          --region us-east-1
#      then return it:
#        aws cloudwatch set-alarm-state --alarm-name "<prefix>-bytes-downloaded" \
#          --state-value OK --state-reason 'synthetic test complete' \
#          --region us-east-1
#
#   d) Verify the BUDGET notification path. AWS Budgets has no test-fire API, so
#      prove it structurally instead:
#        aws budgets describe-notifications-for-budget \
#          --account-id "$(aws sts get-caller-identity --query Account --output text)" \
#          --budget-name '<prefix>-monthly'
#      Expect five notifications: ACTUAL at 50/80/100 and FORECASTED at 80/100.
#      Budget emails come from AWS directly and need no SNS confirmation, but
#      the thresholds must be present or nothing will ever fire.
# ---------------------------------------------------------------------------
section "check 4: synthetic alert [requires deployed stack + AWS creds; opt-in]"

if [ "$fire_synthetic_alert" -eq 0 ]; then
	info "SKIPPED — pass --fire-synthetic-alert to run"
	info "see the manual procedure in the comments of this script"
else
	need aws

	if [ -z "$ALERT_TOPIC_ARN" ]; then
		info "ALERT_TOPIC_ARN unset; trying terraform output"
		ALERT_TOPIC_ARN=$(terraform output -raw guardrails_alert_topic_arn 2>/dev/null || true)
	fi

	if [ -z "$ALERT_TOPIC_ARN" ]; then
		fail "no SNS topic ARN; set ALERT_TOPIC_ARN or run from the terraform dir"
	else
		info "topic: $ALERT_TOPIC_ARN"

		# A pending subscription is the failure this check exists to surface.
		subs=$(aws sns list-subscriptions-by-topic \
			--topic-arn "$ALERT_TOPIC_ARN" \
			--query 'Subscriptions[].SubscriptionArn' \
			--output text 2>/dev/null || true)

		if printf '%s' "$subs" | grep -q 'PendingConfirmation'; then
			fail "SNS subscription is still PendingConfirmation — the recipient must click the confirmation link AWS emailed them, or alerts go nowhere"
		elif [ -z "$subs" ]; then
			fail "topic has no subscriptions"
		else
			pass "SNS subscription is confirmed"
		fi

		if aws sns publish \
			--topic-arn "$ALERT_TOPIC_ARN" \
			--subject 'aish skill mirror: SYNTHETIC guardrail test' \
			--message 'Synthetic alert from guardrail-smoke.sh. No action required. If you are reading this, the cost-alarm notification path works.' \
			>/dev/null 2>&1; then
			pass "published synthetic alert to SNS"
			info "confirm delivery in the inbox of the budget notification recipient"
		else
			fail "aws sns publish failed"
		fi
	fi
fi

# ---------------------------------------------------------------------------
section "summary"

if [ "$failures" -eq 0 ]; then
	printf 'all selected checks passed\n'
	exit 0
fi

printf '%d check(s) failed\n' "$failures" >&2
exit 1
