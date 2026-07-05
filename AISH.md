# AISH.md — agent operating guide

Reference for agents (and humans) working *inside* aish: how the shell surfaces
the right skill for a task, and the I/O disciplines that keep token spend low.
Pairs with [`docs/SKILL-FORMAT.md`](docs/SKILL-FORMAT.md) (skill authoring) and
[`docs/telemetry-efficiency.md`](docs/telemetry-efficiency.md).

## Semantic Skill Matching

Installed skills (`~/.aish/skills/<name>/SKILL.md` and plugin-contributed ones)
are advertised **once** in the system prompt. That static list grows with every
skill you add, so a relevant playbook is easy to overlook on a given turn. aish
closes that gap with a **per-turn semantic match** (`src/skill_match.rs`): it
scores your input against the catalog and, when a skill clearly fits, prepends a
short `[aish skill-awareness] …` note to *that turn's input* pointing the model
at the matching `SKILL.md`. The note goes into the turn input, never the cached
system prompt, so the prompt-cache prefix stays byte-stable.

It's a **hint, not a command** — the model still decides whether to read and
follow the playbook.

### How a skill is ranked

Skills are scored by combining lexical relevance with the semantic frontmatter
(`categories`, `applies-to`, `unwanted-for` — see SKILL-FORMAT.md). Per skill,
in order:

1. **Anti-match (hard suppress).** If a task's implied intent appears in the
   skill's `unwanted-for`, the skill is dropped (score 0) — a release/infra
   playbook never surfaces on a UI-design task.
2. **Keyword relevance (base score).** Distinct task words matched against the
   skill's name (weight 3) and description (weight 1), with prefix-aware
   matching (`review`↔`reviewer`). Works for metadata-free skills too, so
   nothing regresses.
3. **Intent → category boost.** A task's wording implies category tags (e.g.
   "token efficiency" → `performance` + `infrastructure`). Each overlap with the
   skill's declared `categories` adds a strong boost.
4. **Repo-scope multiplier.** When the skill's `applies-to` names the active
   repo, its whole score is multiplied — an in-repo playbook is far more likely
   to be the right one.

The top **2** skills by score (ties broken by name, zeros dropped) are surfaced.
When *no* installed skill fits a substantial task, aish instead **recommends an
installable** one from the bundled registry index (`:skill add <ref>`) rather
than letting the model fake or hand-roll it.

### Inspecting the ranking (debug transparency)

Two ways to see *why* skills ranked the way they did:

- **Interactive:** `:telemetry skill-match <task text>` runs the live scorer
  against your installed catalog and prints each surfaced skill's score plus the
  reasons that moved it (keyword relevance, intent/category boost, applies-to
  multiplier).
- **Per-turn stderr:** export `AISH_SKILL_MATCH_DEBUG=1` (`true`/`on`) to log a
  `[skill-match] <name> score=NN :: <reasons>` line for **every** candidate on
  every turn.

```
$ :telemetry skill-match cut token spend in the coordinator
skill-match reasoning for: cut token spend in the coordinator
  aish_sre                     score= 20  keyword relevance +5; intent/category ["infrastructure"] +5; applies-to 'aish' ×2
  cost-optimization            score=  5  intent/category ["performance"] +5
```

## Ranged I/O (narrow reads by default)

The tool layer and the system prompt both push agents toward **narrow reads** —
re-sending whole large files back through the model is the single biggest
avoidable token cost.

- Reading a file **larger than 5 KB without `line_start`/`line_end` is rejected**
  at the tool layer. Always pass a line range for big files.
- For a large source file, **grep first** to locate the region, **then
  ranged-read** only that slice.
- On grep hits, read just the matched lines plus **~5 lines of context**.
- **Never list a directory with >100 entries** — use a glob or a narrow grep.
- Prefer **one batched block** of ranged reads over re-reading the same file
  end to end.

This discipline is baked into the system prompt as a Core Rule (see
`src/session.rs`) so it applies on the first attempt, not after a rejected bulk
read. The proactive large-file hint (`read_file` on a >5 KB file with no range)
suggests a concrete `line_start`/`line_end` to use.
