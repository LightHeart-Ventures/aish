#!/usr/bin/env bash
# github plugin — `pull_request_review` webhook handler.
#
# STDIN: raw GitHub `pull_request_review` event JSON. Emits a compact
# github-pr-shaped object (schemas/github-pr.json) annotated with the review
# state (approved / changes_requested / commented) so the model can react —
# e.g. auto-merge on approval, or re-run the review skill on changes_requested.
set -euo pipefail

payload="$(cat)"

action="$(printf '%s' "${payload}"  | jq -r '.action // "submitted"')"
number="$(printf '%s' "${payload}"  | jq -r '.pull_request.number // 0')"
title="$(printf '%s' "${payload}"   | jq -r '.pull_request.title // ""')"
author="$(printf '%s' "${payload}"  | jq -r '.pull_request.user.login // ""')"
branch="$(printf '%s' "${payload}"  | jq -r '.pull_request.head.ref // ""')"
base="$(printf '%s' "${payload}"    | jq -r '.pull_request.base.ref // ""')"
url="$(printf '%s' "${payload}"     | jq -r '.pull_request.html_url // ""')"
review_state="$(printf '%s' "${payload}" | jq -r '.review.state // ""')"
reviewer="$(printf '%s' "${payload}"     | jq -r '.review.user.login // ""')"

jq -n \
  --arg event  "pull_request_review" \
  --arg action "${action}" \
  --argjson number "${number:-0}" \
  --arg title  "${title}" \
  --arg author "${author}" \
  --arg branch "${branch}" \
  --arg base   "${base}" \
  --argjson draft false \
  --arg url    "${url}" \
  --arg review_state "${review_state}" \
  --arg reviewer "${reviewer}" \
  '{event:$event, action:$action, number:$number, title:$title,
    author:$author, branch:$branch, base:$base, draft:$draft, url:$url,
    review_state:$review_state, reviewer:$reviewer}'
