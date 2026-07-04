#!/usr/bin/env bash
# github plugin — `issues` webhook handler.
#
# STDIN: raw GitHub `issues` event JSON. Emits an object matching
# schemas/github-issue.json on STDOUT for the model to triage (optionally via
# the github-issue-triage skill). When the plugin's `auto_comment_on_open`
# config is true AND action==opened, aish is signalled (needs_ack:true) to run
# the add_comment tool — the handler itself stays read-only.
set -euo pipefail

payload="$(cat)"

action="$(printf '%s' "${payload}" | jq -r '.action // "unknown"')"
number="$(printf '%s' "${payload}" | jq -r '.issue.number // 0')"
title="$(printf '%s' "${payload}"  | jq -r '.issue.title // ""')"
author="$(printf '%s' "${payload}" | jq -r '.issue.user.login // ""')"
state="$(printf '%s' "${payload}"  | jq -r '.issue.state // ""')"
labels="$(printf '%s' "${payload}" | jq -c '[.issue.labels[]?.name]')"
url="$(printf '%s' "${payload}"    | jq -r '.issue.html_url // ""')"

needs_ack="false"
if [[ "${action}" == "opened" && "${GITHUB_AUTO_COMMENT_ON_OPEN:-false}" == "true" ]]; then
  needs_ack="true"
fi

jq -n \
  --arg event  "issues" \
  --arg action "${action}" \
  --argjson number "${number:-0}" \
  --arg title  "${title}" \
  --arg author "${author}" \
  --arg state  "${state}" \
  --argjson labels "${labels:-[]}" \
  --arg url    "${url}" \
  --argjson needs_ack "${needs_ack}" \
  '{event:$event, action:$action, number:$number, title:$title,
    author:$author, state:$state, labels:$labels, url:$url, needs_ack:$needs_ack}'
