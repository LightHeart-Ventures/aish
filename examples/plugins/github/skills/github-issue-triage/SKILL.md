---
name: github-issue-triage
description: Triage a newly opened or untriaged GitHub issue — classify it, apply a label from the configured vocabulary, assess severity, and (with confirmation) post an acknowledgement comment. Triggered by the issues webhook handler or run manually over the open-issue backlog.
version: 1.0.0
provides_tools: [list_issues, add_comment]
mcp_server: github
categories: [review, troubleshooting]
applies-to: [all]
unwanted-for: [design]
---

# GitHub Issue Triage

Playbook for triaging issues on this plugin's configured repo.

## When to use
- The `issues` webhook handler emitted a `github-issue` observation with action
  `opened` (and possibly `needs_ack: true`).
- A user asks to triage the open-issue backlog (use `list_issues` to pull it).

## Label vocabulary
Apply labels ONLY from `config.triage_labels` (default:
`bug`, `enhancement`, `question`, `needs-triage`, `security`). Never invent labels.

## Steps
1. **Read** the issue title + body from the observation (or `list_issues`).
2. **Classify** into exactly one primary type:
   - `bug` — reproducible defect. Note whether repro steps are present.
   - `enhancement` — feature / improvement request.
   - `question` — usage/support; often closeable with a pointer to docs.
   - `security` — anything smelling of a vulnerability → escalate, do NOT
     discuss specifics in a public comment.
   - `needs-triage` — genuinely can't classify without more info.
3. **Severity** (bugs only): critical / high / medium / low, by blast radius.
4. **Act** (with confirmation):
   - Apply the chosen label via the GitHub MCP `add_labels` tool.
   - If `needs_ack` is set, post a brief acknowledgement via `add_comment`
     using the template below.
   - If it's a `question` answerable from the README, answer and suggest closing.

## Acknowledgement template
```
Thanks for the report, @<author>! Triaged as **<label>**<, severity **<sev>**>.
<one line: next step, or what info we still need>
```

## Guardrails
- One label from the allowed set — no free-form labels, no label storms.
- Never post vulnerability details publicly; for `security`, comment only
  "Thanks — following up privately" and flag for a maintainer.
- Don't close issues automatically; recommend closure, let a human confirm.
