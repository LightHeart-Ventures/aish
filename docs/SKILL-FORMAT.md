# SKILL.md format

A skill is a directory containing a `SKILL.md` file: YAML frontmatter followed
by a markdown body. aish discovers installed skills under `~/.aish/skills/<name>/`
and plugin-contributed skills under `~/.aish/plugins/<id>/skills/<name>/`, then
advertises them in the system prompt so the model reads and follows them on
demand.

## Frontmatter fields

| Field | Required | Type | Purpose |
|-------|----------|------|---------|
| `name` | yes | string | Unique skill id (matches the directory name by convention). |
| `description` | yes | string | One- or few-line summary; drives skill-awareness matching. |
| `categories` | recommended | array | Coarse topic buckets the skill belongs to. |
| `applies-to` | recommended | array | Repo/project scopes the skill is meant for. |
| `unwanted-for` | optional | array | Intent patterns to SUPPRESS the skill on. |

`name` and `description` may be single-line or use YAML block scalars
(`description: >`). Other frontmatter keys (`version`, `allowed-tools`,
`mcp_server`, `provides_tools`, `argument-hint`, `user-invocable`, …) are
preserved but ignored by the semantic matcher.

## Semantic metadata (TASK-331)

Three list fields enable semantic skill-matching — scoring a skill against the
current task's topic, the active repo, and the user's intent so the per-turn
`[aish skill-awareness]` nudge fires on the right skill and stays quiet on the
wrong one.

### `categories`

Coarse topic buckets. Open vocabulary; common values:

`infrastructure`, `troubleshooting`, `release`, `perf`, `design`, `review`,
`docs`, `discovery`, `security`, `testing`, `database`.

### `applies-to`

Repo/project scopes the skill targets. Use `all` for broadly-applicable skills,
or specific project slugs otherwise:

`aish`, `cloudinero`, `all`, …

An empty `applies-to` is treated as unscoped (broadly applicable), same as `all`.

### `unwanted-for`

Intent patterns the skill should be *suppressed* on. When the user's intent
matches one of these, the skill is filtered out before the nudge fires — a
release/infra playbook shouldn't surface on a UI-design task.

`review`, `design`, `ui`, `infrastructure`, `perf`, …

## Accepted YAML shapes

Both list shapes parse identically. Inline flow sequence:

```yaml
categories: [infrastructure, troubleshooting, release]
applies-to: [aish]
unwanted-for: [design, review]
```

Block sequence:

```yaml
categories:
  - infrastructure
  - troubleshooting
  - release
applies-to:
  - aish
unwanted-for:
  - design
  - review
```

Items are trimmed and a single pair of surrounding quotes is stripped, so
`- "code-quality"` and `- 'all'` are accepted. Empty entries are dropped.

## Validation

On startup aish parses the metadata for every installed skill. Missing fields
are **non-fatal**: a skill without `categories`/`applies-to` still loads and
functions. aish emits a yellow startup notice naming any skill missing both, so
authors can backfill against this document. There is no crash on a pre-schema
SKILL.md — the three fields simply default to empty arrays.

## Complete example

```markdown
---
name: aish_sre
description: Site-reliability & troubleshooting playbook for aish.
categories: [infrastructure, troubleshooting, release]
applies-to: [aish]
unwanted-for: [design, review]
---

# aish SRE

Body of the playbook…
```
