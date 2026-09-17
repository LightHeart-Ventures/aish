# gstack Plugin — Installation Guide

This plugin registers **[gstack](https://github.com/garrytan/gstack)** as an aish
**skill source**, so `:skill search` finds gstack skills and `:skill add
gstack:<name>` installs them.

---

## Prerequisites

### `jq` and `curl`

Both handlers are POSIX shell + `jq`. `curl` is only needed for `:skill add`
and `refresh-catalog.sh`.

```sh
# macOS
brew install jq curl

# Debian/Ubuntu
apt-get install -y jq curl
```

Check:

```sh
jq --version      # jq-1.6 or newer
curl --version
```

**No Node, no Python, no gstack install required** — the plugin talks to GitHub
directly. (Individual gstack skills may later ask for gstack-specific tooling
like `gbrain` or the GStack Browser; that is an upstream concern, not a plugin
prerequisite.)

---

## Verification

### Step 1: plugin is discovered

```sh
ls ~/.aish/plugins/gstack/
# plugin.json  search.sh  add.sh  catalog.json  refresh-catalog.sh  README.md  skills/
```

From inside aish:

```
:skill sources
```

`gstack` should be listed (priority 90).

### Step 2: search returns gstack rows

```
:skill search ship
```

Expected: at least one row with `SOURCE = gstack`, e.g.

| NAME | SOURCE | DESCRIPTION |
|---|---|---|
| ship | gstack | Ship workflow: detect + merge base branch, run tests, review diff, bump VERSION… |

### Step 3: handlers run standalone

```sh
cd ~/.aish/plugins/gstack
AISH_SKILL_QUERY=design ./search.sh | jq 'length'     # > 0
AISH_SKILL_REF=gstack:ship ./add.sh | head -5          # YAML frontmatter
```

### Step 4: install a skill

```
:skill add gstack:ship
:skill list
```

`ship` should now appear in `~/.aish/skills/`.

---

## Installation Summary

| Step | Command | Expected |
|---|---|---|
| 1. Install deps | `brew install jq curl` | versions print |
| 2. Confirm source | `:skill sources` | `gstack` listed |
| 3. Search | `:skill search qa` | rows with `SOURCE=gstack` |
| 4. Add | `:skill add gstack:qa` | skill written to `~/.aish/skills/qa` |

---

## Configuration

```sh
# Pin imports to a tag instead of main (reproducible installs)
export AISH_GSTACK_REF=v1.2.0

# Point search at a different catalog snapshot
export AISH_GSTACK_CATALOG=/path/to/catalog.json

# Only for refresh-catalog.sh — avoids the 60 req/h anonymous GitHub limit
export GITHUB_TOKEN=ghp_…
```

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `:skill search` shows no gstack rows | `jq` missing, or catalog unreadable | `jq --version`; `ls -l ~/.aish/plugins/gstack/catalog.json` |
| `gstack` absent from `:skill sources` | Plugin not discovered | `:restart`, then re-check `ls ~/.aish/plugins/gstack/plugin.json` |
| `:skill add` → "unknown gstack skill" | Name not in the catalog (upstream renamed it) | `./refresh-catalog.sh`, then retry |
| `:skill add` → "fetch failed" | Bad `AISH_GSTACK_REF`, or network/GitHub outage | `unset AISH_GSTACK_REF`; confirm the ref exists upstream |
| `refresh-catalog.sh` → API 403 | Anonymous GitHub rate limit | `export GITHUB_TOKEN=…` and retry |
| Handler "permission denied" | Exec bit lost (zip/copy) | `chmod +x ~/.aish/plugins/gstack/*.sh` |

---

## Next Steps

1. **Browse the suite:** `:skill search gstack`
2. **Install the router skill** so aish can map intent → gstack skill:
   `:skill add gstack:gstack`
3. **Install the whole suite:** `:skill add gstack:*`
4. **Read upstream docs:** https://github.com/garrytan/gstack

---

## Links

- **Plugin dir:** `~/.aish/plugins/gstack/`
- **gstack upstream:** https://github.com/garrytan/gstack
- **Skill-source design doc:** `docs/design/plugin-skill-sources.md`
- **aish docs:** https://github.com/LightHeart-Ventures/aish
