# vercel-skills — a JSON-first fork of [vercel-labs/skills](https://github.com/vercel-labs/skills)

An aish plugin that reimplements the **core of the open agent-skills ecosystem
tool** ([`skills.sh`](https://skills.sh) / `npx skills`) as a **single
zero-dependency Node script whose only job is structured JSON**.

Upstream is a mature, human-facing CLI (install / sync / update / remove across
70+ agents). This fork keeps the part that is reusable infrastructure —
**discovering `SKILL.md` files and parsing them into a clean data model** — and
drops everything else, so any tool (aish, an IDE, an MCP router, a CI job) can
get a machine-readable skill catalog without shelling out to a formatted CLI or
pulling a `node_modules` tree.

> Why a fork and not just `npx skills list --json`? See **[REVIEW.md](./REVIEW.md)**.
> Short version: upstream's `--json` exists only on `list` and only reports
> *installed* skills per agent; it can't parse an arbitrary directory tree into
> a catalog, it requires the full npm package + network/telemetry init, and its
> data model isn't exposed as a library. This fork is that missing library, lean.

## What it does

Point it at a directory tree; it recursively finds every `SKILL.md`, parses the
YAML frontmatter (`name`, `description`, `metadata`) + markdown body, and emits
JSON.

```
skills-json list    [dir...]            # catalog: name, description, path, metadata
skills-json find    <query> [dir...]    # same, filtered over name/description/body
skills-json use     <name>  [dir...]    # ONE skill incl. its body (the prompt text)
skills-json catalog [dir...]            # full catalog with body + contentHash
```

Flags: `--include-body`, `--include-internal`, `--full-depth`, `--max-depth <n>`,
`--compact` / `--pretty`, `--help`.

Default search dir when none is given: `$AISH_SKILLS_DIR`, else `~/.aish/skills`,
else the current directory.

### Examples

```bash
# Every skill aish has installed, as JSON
./skills-json.sh list ~/.aish/skills

# Find skills related to postgres, across two trees
./skills-json.sh find postgres ~/.aish/skills ./.claude/skills

# Resolve one skill to its full prompt body (what `skills use` prints, but JSON)
./skills-json.sh use sprint-status --include-body
```

Sample `use` output:

```json
{
  "name": "sprint-status",
  "description": "Summarize the current sprint board …",
  "path": "/Users/you/.aish/skills/sprint-status",
  "relativePath": "sprint-status",
  "source": "/Users/you/.aish/skills",
  "metadata": {},
  "contentHash": "1f3c…",
  "body": "# sprint-status — summarize the active sprint\n…"
}
```

## Design (faithful, lean)

| Upstream (`src/…`) | This fork | Note |
|---|---|---|
| `frontmatter.ts` | `parseFrontmatter()` | Same YAML-delimiter-only regex; **no** `---js` engine (no eval RCE). |
| `yaml` npm package | `parseYamlLite()` | Minimal indentation-aware YAML subset → **zero runtime deps**. |
| `types.ts` `Skill` | JSON record | `name, description, path, metadata` (+ `body`, `relativePath`, `contentHash`). |
| `skills.ts` discovery | `findSkillDirs()` / `discover()` | Same recursive walk + `SKIP_DIRS`; required-string `name`/`description`; `metadata.internal` gating. |
| `list/find/use.ts` | `list/find/use/catalog` | JSON-only, no ANSI, no telemetry, no network, no install side-effects. |

**Deliberately out of scope** (that's upstream's job): installing, syncing,
removing, updating, agent detection, lockfiles, remote providers, telemetry.
This is a read-only parser/emitter.

## Requirements

`node >= 18` on `PATH`. No `npm install`, no build step, no `node_modules`.

## Install (runtime)

```bash
cp -r plugins/vercel-skills ~/.aish/plugins/
~/.aish/plugins/vercel-skills/skills-json.sh list
```

## License & attribution

Fork of [vercel-labs/skills](https://github.com/vercel-labs/skills) (**MIT**),
forked at upstream `v1.5.15`. The reimplemented parsing/discovery logic follows
upstream's algorithms; the plugin wrapper is Apache-2.0. See
[NOTICE](./NOTICE).
