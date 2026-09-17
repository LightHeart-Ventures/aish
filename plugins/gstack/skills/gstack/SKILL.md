---
name: gstack
description: Route a task to the right gstack skill (garrytan/gstack) — plan/spec/ship/review, design consultation + review, QA, iOS live-device workflows, security audit, docs, browser automation. Use when the user says "gstack", names a gstack command (/plan, /ship, /review, /spec, /qa, /cso, …), or asks which gstack skill fits a task.
---

# gstack router

[gstack](https://github.com/garrytan/gstack) is Garry Tan's agent-skill suite:
54 skills that encode a full product loop — spec → plan → review → ship →
QA → docs → retro. This plugin makes them first-class in aish:

- `:skill search <query>` fans out to the gstack catalog (offline, instant).
- `:skill add gstack:<name>` pulls that skill's `SKILL.md` from GitHub into
  `~/.aish/skills/` — from then on it is a normal installed aish skill.
- `:skill add gstack:*` installs the whole suite.

## How to use this skill

1. **Identify the phase** the user is in (see the map below).
2. **Check whether the skill is installed** — `list_dir ~/.aish/skills`. If it is,
   `read_file` its `SKILL.md` and follow it; that is what "using a skill" means.
3. **If it is not installed**, tell the user the exact command:
   `:skill add gstack:<name>` — do not hand-roll a lesser version of it.
4. gstack SKILL.md files assume Claude Code conventions (`/slash` commands,
   `AskUserQuestion`, `Bash`). In aish, translate: slash command → read the
   SKILL.md and execute its steps; `Bash` → `run_program`; `Read`/`Write` →
   `read_file`/`write_file`; `AskUserQuestion` → just ask in your reply.

## Intent → skill map

| You want to… | gstack skill |
|---|---|
| Turn vague intent into an executable spec | `spec` |
| Review a plan as CEO / eng manager / designer / DX | `plan-ceo-review`, `plan-eng-review`, `plan-design-review`, `plan-devex-review` |
| Run every plan review back-to-back, auto-decided | `autoplan` |
| Review a PR before landing | `review` |
| Merge base, test, bump, changelog, push, open PR | `ship` |
| Land + deploy, then watch a canary | `land-and-deploy`, `canary` |
| Systematic debugging with root-cause discipline | `investigate` |
| Code-quality dashboard / perf regressions | `health`, `benchmark` |
| QA a web app (fix vs report-only) | `qa`, `qa-only` |
| Design system, HTML, visual QA, variants | `design-consultation`, `design-html`, `design-review`, `design-shotgun` |
| iOS live-device QA / autonomous fixes | `ios-qa`, `ios-fix`, `ios-design-review`, `ios-sync`, `ios-clean` |
| Security audit | `cso` |
| Generate or refresh docs after shipping | `document-generate`, `document-release` |
| Browser automation / scraping with real sessions | `browse`, `scrape`, `skillify`, `setup-browser-cookies` |
| Diagrams, PDFs | `diagram`, `make-pdf` |
| Guardrails on destructive commands / edit scope | `careful`, `freeze`, `unfreeze`, `guard` |
| Weekly retro, YC-style office hours | `retro`, `office-hours` |
| Save / restore working context | `context-save`, `context-restore` |

Full, always-current list: `:skill search gstack` (or read
`plugins/gstack/catalog.json`).

## aish-native equivalents (prefer these when they exist)

Some gstack skills overlap aish's own bundled skills. Prefer the aish one when
the task is aish-internal, and gstack's when the user explicitly asks for gstack:

| gstack | aish equivalent |
|---|---|
| `review` | `pr-reviewer`, `code-review-checklist` |
| `cso` | `security-audit` |
| `health` | `test-coverage`, `dependency-audit` |
| `investigate` | `incident-responder`, `ci-debugger` |
| `retro` | `sprint-status`, `sprint-snapshot` |

## Caveats

- gstack skills may shell out to gstack-specific tooling (`gbrain`, the GStack
  Browser, Aside, DebugBridge). Those steps need the upstream install —
  see https://github.com/garrytan/gstack. The *reasoning* steps work standalone.
- Imports are pinned by `AISH_GSTACK_REF` (default `main`). Set it to a tag for
  reproducible installs.
- gstack skills are licensed by their upstream repo, not by aish.
