---
name: github-pr-review
description: Review a GitHub pull request end-to-end — fetch metadata + diff, assess correctness/security/style, and (with confirmation) post a structured review via add_comment. Triggered by the pull_request webhook handler or invoked manually with a PR number.
version: 1.0.0
provides_tools: [list_issues, add_comment]
mcp_server: github
categories: [review]
applies-to: [all]
unwanted-for: [infrastructure, perf]
---

# GitHub PR Review

Expert playbook for reviewing a pull request on this plugin's configured repo.

## When to use
- The `pull_request` webhook handler emitted a `github-pr` observation with
  action `opened`, `synchronize`, or `ready_for_review`.
- A user asks to review PR #N.

## Inputs
- **number** — PR number (from the webhook observation or the user).
- Repo `owner`/`repo` come from plugin config; the GitHub MCP token from
  `[profile:github]`.

## Steps
1. **Fetch context.** Use the `github` MCP server's `get_pull_request` and
   `get_pull_request_files` for metadata + the unified diff. Skip vendored/lock
   files and generated output.
2. **Assess** across four axes, highest-severity first:
   - **Correctness** — logic errors, off-by-one, unhandled error paths, races.
   - **Security** — injection, secret leakage, authz gaps, unsafe deserialization.
   - **Tests** — do changes ship with coverage? Do existing tests still hold?
   - **Style** — only flag what a linter wouldn't already catch.
3. **Decide a verdict**: `approve`, `comment`, or `request_changes`.
4. **Post** (after confirmation) a single structured review comment via
   `add_comment` using the template below. One comment, not a scatter of nits.

## Review comment template
```
## aish review — <verdict>

**Summary:** <one-line take>

### Blocking
- <file:line> <issue> — <why it matters>

### Non-blocking
- <file:line> <suggestion>

### Tests
<present / missing / gaps>
```

## Guardrails
- Never merge or close a PR from this skill — review only.
- If the diff is >1500 changed lines, review the highest-risk files and say so
  explicitly rather than skimming everything.
- No praise-padding. Signal only.
