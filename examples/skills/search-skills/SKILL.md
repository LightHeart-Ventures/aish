---
name: search-skills
description: >
  Find and recommend the best skill for a task. Use when the user asks to find,
  search, or recommend a skill, or asks "what skill helps with…" / "find me a
  skill to…". Ranks INSTALLED skills by relevance, and when none fits, recommends
  an INSTALLABLE one from the offline registry index. This is the user-invoked,
  richer-UI sibling of aish's automatic per-turn skill-awareness (the
  `[aish skill-awareness]` banner) — it does not replace it.
user-invocable: true
argument-hint: "<task description or keywords>"
allowed-tools: Bash(${CLAUDE_PLUGIN_ROOT}/scripts/*), Bash(find ~/.aish/skills -name "SKILL.md"), Read, Grep
---

# Search & Recommend Skills

Announce: "I'll find the best-fitting skill for that."

> **How this fits the rest of aish.** aish already nudges you automatically:
> every turn, the engine (`skill_match`) matches the task against your installed
> skills and, when one fits, prepends an `[aish skill-awareness]` note pointing
> at its `SKILL.md`; when nothing local fits a substantial task it recommends an
> installable registry skill (`:skill add <ref>`). This skill is the **explicit,
> richer-output** counterpart for when the user *asks* "find me a skill" — it
> shows a ranked table with star ratings and a clear recommendation. It reads
> the **same** offline registry index the engine uses, so the two never
> disagree about what's installable. For a **live network search** across
> skill.fish / mcpmarket, use the built-in `:skill search <query>` command — this
> skill deliberately does NOT shadow that verb.

## Using a skill (read this first)

"Using" a skill means **reading its `SKILL.md` and carrying out its steps
yourself with your normal tools** — there is no separate command to "invoke" a
skill, and you must never claim you can't run one. So when this playbook lands on
a recommendation, either follow the recommended installed skill, or surface the
`:skill add <ref>` recommendation — never fake or silently hand-roll a skill
that isn't installed.

## Step 1: Understand the Task

Parse the user's request to identify:
- **Core task category** (git conflicts, AWS Lambda, CI/CD, testing, docs, …).
- **Keywords**: 3–5 primary keywords (split on spaces/punctuation, lowercase,
  drop stop words like "the", "a", "to", "with").
- **Context clues**: technologies, services, or pain points mentioned.

## Step 2: Rank Installed Skills

List installed skills and score them by relevance to the task:

```bash
${CLAUDE_PLUGIN_ROOT}/scripts/discover-skills.sh "<task description>"
```

The script scans `~/.aish/skills/*/SKILL.md`, extracts each skill's `name` +
`description`, and scores it with the **same name-weighted relevance rule the
engine uses** (`skill_match::relevance`): for each distinct task keyword, a
NAME-token match outweighs a DESCRIPTION-token match. Deferring to that one rule
keeps this skill's ranking consistent with the automatic `[aish skill-awareness]`
banner instead of inventing a second, competing score.

## Step 3: Attach Star Ratings (best-effort)

```bash
${CLAUDE_PLUGIN_ROOT}/scripts/registry-stars.sh <skill-name-1> <skill-name-2> …
```

Stars come from the offline registry index (`~/.aish/registry/index.json`) when
present, else 0. This is a presentation nicety only — never let a star count
override a clearly-better keyword match. Network is never required.

## Step 4: When No Installed Skill Fits — Recommend an Installable One

If the top installed score is weak (no real keyword overlap), look for an
installable candidate in the offline registry — the exact source the engine's
`recommend_install` reads:

```bash
${CLAUDE_PLUGIN_ROOT}/scripts/registry-candidates.sh "<task description>"
```

Pick the best registry match (same relevance rule) that ISN'T already installed,
and recommend it with its `reference`:

```
ℹ️  No installed skill fits "<task>". The registry has a relevant one:

   anthropic/kubernetes-deploy — Deploy applications to Kubernetes clusters.

   Install it:  :skill add anthropic/kubernetes-deploy
   Then I'll read its SKILL.md and follow it.

   (For a live search across skill.fish / mcpmarket: :skill search <keywords>)
```

Do NOT pretend to run, or silently re-implement, a skill that isn't installed —
surface the recommendation instead.

## Step 5: Present the Result

For installed matches, output a ranked table:

```
Top skills for "<task>":

| Rank | Skill                       | Stars | Match | Description                         |
|------|-----------------------------|-------|-------|-------------------------------------|
| 1    | fix-conflicts (INSTALLED)   | ⭐⭐⭐⭐ | high  | Merge/rebase conflict resolution…   |
| 2    | aws-serverless-eda (INST.)  | ⭐⭐⭐  | low   | Lambda & event-driven patterns…     |
```

Then state the recommendation plainly:

- **Strong installed match** → name it and offer to follow it now:
  "I'll follow the `fix-conflicts` skill — reading its `SKILL.md` and carrying
  out its steps."
- **Weak / no installed match** → give the Step 4 install recommendation.

## Error Handling

| Situation                       | Action                                                                 |
|---------------------------------|------------------------------------------------------------------------|
| No installed skills found       | Skip straight to Step 4 (registry recommendation) or `:skill search`.  |
| Registry index missing          | Stars/candidates degrade to empty; rank installed skills only.         |
| Malformed `SKILL.md`            | Skip that skill with a note; continue with the rest.                   |
| Keywords unclear                | Ask the user to describe the task more specifically.                   |

## Example Interactions

### Strong installed match

```
User: "find a skill to help with git merge conflicts"

✅ fix-conflicts (INSTALLED) — high relevance
   Resolves merge/rebase/cherry-pick/revert conflicts with intent preservation.
   I'll follow it now: reading its SKILL.md and carrying out its steps.
```

### No installed match → recommend install

```
User: "find a skill for deploying to kubernetes"

ℹ️  No installed skill fits. Registry match:
   anthropic/kubernetes-deploy — Deploy applications to Kubernetes clusters.
   Install:  :skill add anthropic/kubernetes-deploy
   Live search instead:  :skill search kubernetes deploy
```
