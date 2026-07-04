#!/usr/bin/env bash
# github plugin — `pull_request` webhook handler.
#
# INVOCATION
#   Forked by aish when a GitHub `pull_request` event arrives at the broker.
#   The raw JSON payload is on STDIN. Env: $GITHUB_EVENT=pull_request,
#   $GITHUB_DELIVERY=<uuid>, plus $GITHUB_OWNER/$GITHUB_REPO.
#
# CONTRACT
#   * Emit a single JSON object on STDOUT conforming to schemas/github-pr.json.
#   * aish validates that object against the schema and feeds it to the model
#     as a structured observation (optionally kicking the github-pr-review skill).
#   * Exit 0 on success. Non-zero => delivery marked failed (broker may retry).
#
# `jq` is assumed present (documented dependency). Keep handlers pure-stdin →
# stdout; do not mutate the repo here — routing/decisioning is the model's job.
set -euo pipefail

payload="$(cat)"

action="$(printf '%s' "${payload}"  | jq -r '.action // "unknown"')"
number="$(printf '%s' "${payload}"  | jq -r '.number // .pull_request.number // 0')"
title="$(printf '%s' "${payload}"   | jq -r '.pull_request.title // ""')"
author="$(printf '%s' "${payload}"  | jq -r '.pull_request.user.login // ""')"
branch="$(printf '%s' "${payload}"  | jq -r '.pull_request.head.ref // ""')"
base="$(printf '%s' "${payload}"    | jq -r '.pull_request.base.ref // ""')"
draft="$(printf '%s' "${payload}"   | jq -r '.pull_request.draft // false')"
url="$(printf '%s' "${payload}"     | jq -r '.pull_request.html_url // ""')"

jq -n \
  --arg event   "pull_request" \
  --arg action  "${action}" \
  --argjson number "${number:-0}" \
  --arg title   "${title}" \
  --arg author  "${author}" \
  --arg branch  "${branch}" \
  --arg base    "${base}" \
  --argjson draft "${draft:-false}" \
  --arg url     "${url}" \
  '{event:$event, action:$action, number:$number, title:$title,
    author:$author, branch:$branch, base:$base, draft:$draft, url:$url}'
