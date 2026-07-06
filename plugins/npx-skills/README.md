# npx-skills — a `provides.skill_source` reference plugin

Wraps the community [`npx skills`](https://www.npmjs.com/package/skills) CLI
(the agentskills.io / skill.fish ecosystem) as an aish **skill source**, so
`:skill search` and `:skill add` can discover and install community skills
without that source being compiled into the aish binary.

It is the reference implementation of the **script** skill-source shape from
`docs/design/plugin-skill-sources.md` (§4 shape B) and the companion authoring
guide `docs/plugins/skill-source-authoring.md`.

## What it contributes

| Manifest field | Value | Effect |
|---|---|---|
| `provides.skill_source.id` | `npx-skills` | SOURCE label in `:skill search` / `:skill sources` |
| `priority` | `90` | Ranks below the built-in `skillfish` façade (100), above unranked sources |
| `search` | `search.sh` | Answers `:skill search <q>` via `npx skills search --json` |
| `add` | `add.sh` | Resolves `:skill add npx:<spec>` via `npx skills add` |
| `handles` | `["npx:*","skills:*"]` | Claims the `npx:` / `skills:` ref namespaces for add-routing |

## Install

Copy this directory into your plugins dir:

```
cp -r plugins/npx-skills ~/.aish/plugins/
```

Then, from aish:

```
:skill sources            # npx-skills now listed
:skill search postgres    # results include SOURCE=npx-skills rows
:skill add npx:owner/repo/some-skill
```

## Handler contract (recap)

Both scripts follow the same contract as `login.sh`
(`docs/design/plugin-skill-sources.md` §3.1):

- **`search.sh`** — reads `AISH_SKILL_QUERY`, prints a JSON array of
  `SearchResult` objects (`name, author, description, version, reference, stars`)
  on stdout. Fail-soft: any error yields `[]` and exit 0, so a broken source
  never breaks the fan-out.
- **`add.sh`** — reads `AISH_SKILL_REF` (+ `AISH_SKILLS_DIR`), installs into a
  scratch dir, and prints a JSON array of `{ path, content }` SKILL.md records.
  The REPL performs the write + catalog reload.

## Dependencies

`node`/`npx` and `jq` on `PATH`. Absent deps degrade `search.sh` to `[]` and
cause `add.sh` to fail with a clear message.

## Notes / caveats

- The `npx skills` CLI's machine-readable output is still stabilising; the
  scripts normalise several field shapes defensively but may need a tweak as
  that CLI evolves — this is exactly the win of shipping a source as an
  **updatable script plugin** instead of compiled-in code.
- `search.sh` is fail-soft by design; `add.sh` is fail-loud (a user asked to
  install a specific ref, so a failure must surface).
