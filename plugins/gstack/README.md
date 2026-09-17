# gstack plugin

Brings the [gstack](https://github.com/garrytan/gstack) skill suite (Garry Tan)
into aish as a **skill source** — 54 skills spanning spec → plan → review →
ship → QA → docs → retro, plus design, iOS live-device, security, and browser
automation workflows.

## What it provides

| Capability | How |
|---|---|
| `:skill search <q>` includes gstack results | `search.sh` (offline catalog, priority 90) |
| `:skill add gstack:<name>` installs one skill | `add.sh` → raw.githubusercontent.com |
| `:skill add gstack:*` installs the whole suite | `add.sh` multi-import |
| Intent → skill routing inside aish | `skills/gstack/SKILL.md` |

## Usage

```
:skill search ship            # gstack rows appear with SOURCE=gstack
:skill add gstack:ship        # one skill
:skill add gstack:spec
:skill add gstack:*           # all 54 + the router
:skill list
```

Reference forms `add.sh` accepts:

| Reference | Result |
|---|---|
| `gstack:ship` | that one skill |
| `gstack:gstack` | the upstream top-level router SKILL.md |
| `gstack:*` / `gstack:all` / `garrytan/gstack` | full suite (multi-import) |
| `garrytan/gstack/ship` | same as `gstack:ship` |

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `AISH_GSTACK_REF` | `main` | Git ref imports are fetched from. Pin to a tag for reproducibility. |
| `AISH_GSTACK_CATALOG` | `catalog.json` | Override the offline search catalog path. |
| `GITHUB_TOKEN` | — | Only used by `refresh-catalog.sh` to dodge the anonymous API rate limit. |

## Refreshing the catalog

Search is served from a bundled `catalog.json` so it is instant and works
offline. When upstream adds or renames skills:

```sh
./refresh-catalog.sh              # against main
REF=v1.2.0 ./refresh-catalog.sh   # against a tag
```

It walks the upstream tree, reads each `<dir>/SKILL.md` frontmatter, and
rewrites `catalog.json`.

## Design notes

- **Search is offline, add is online.** Search hits no network (no rate limit,
  no latency in the `:skill search` fan-out); only `:skill add` fetches.
- **Fail-soft search.** Missing `jq`, unreadable catalog, or malformed JSON →
  `[]` + exit 0, so the fan-out degrades to its other sources rather than
  erroring (same posture as `npx-skillfish`).
- **Loud add.** `:skill add` failures are surfaced, not swallowed — an import
  that silently no-ops is worse than one that explains itself.
- **Priority 90** — above the built-in embedded index, below `npx-skillfish`
  (100/85 band), so gstack wins name-ties against the built-in catalog.

## Translating gstack skills to aish

gstack SKILL.md files are written for Claude Code. When you follow one in aish:

| gstack / Claude Code | aish |
|---|---|
| `/slash` command | read the SKILL.md, execute its steps |
| `Bash` tool | `run_program` (no shell syntax — no pipes/globs/redirection) |
| `Read` / `Write` | `read_file` / `write_file` |
| `AskUserQuestion` | just ask in your reply |

Some skills shell out to gstack-specific tooling (`gbrain`, GStack Browser,
Aside, DebugBridge). Those steps need the upstream install; the reasoning steps
work standalone.

## License

Plugin wrapper: Apache-2.0 (same as aish).
gstack skills: licensed by their upstream repo — see
https://github.com/garrytan/gstack. This plugin ships **no** upstream skill
content; it fetches it at `:skill add` time and records attribution to
`garrytan`.
