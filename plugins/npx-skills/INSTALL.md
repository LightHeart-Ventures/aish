# npx-skills Plugin — Installation Guide

This plugin enables **npm/npx skill management** in aish, allowing you to search for, add, and manage skills from npm registries and GitHub repositories directly within the aish REPL.

---

## Prerequisites

### Node.js / npx

The plugin uses **`npx`** to run skill-related commands. You'll need:

```sh
# Check if you have Node.js + npm
node --version
npm --version

# If not, install Node.js (includes npm)
# macOS:
brew install node

# Linux:
apt-get install nodejs npm

# Windows:
# Download from https://nodejs.org/
```

**That's it.** The plugin auto-downloads dependencies on first use via `npx`.

---

## Verification

### Step 1: Node.js is installed

```sh
which npm
npm --version
# Should print a version number (e.g., 10.2.4)
```

### Step 2: aish plugin is discovered

From inside aish:

```
:mcp
```

You should see:

```
npx-skills (local)
  Tools: 2
    - skill_search
    - skill_add
```

Or check directly:

```sh
ls ~/.aish/plugins/npx-skills/
# Should show: plugin.json, README.md, search.sh, add.sh
```

### Step 3: Try a skill search

From inside aish:

```
skill_search { query: "design" }
```

Expected output: A list of available skills matching "design".

---

## Installation Summary

| Step | Command | Expected Output |
|------|---------|-----------------|
| 1. Install Node.js | `brew install node` (or system package manager) | `npm --version` prints version |
| 2. Check aish plugin | `:mcp` from inside aish | `npx-skills` appears in MCP server list |
| 3. Test a search | `skill_search { query: "..." }` | Skill list returned |

---

## Configuration

### Environment Variables

```sh
# Optional: set npm registry
export NPM_REGISTRY="https://registry.npmjs.org"  # Default: npm public registry

# Optional: set GitHub token for private repos
export GITHUB_TOKEN="ghp_..."  # If searching private skill repos
```

### Command Usage

From inside aish:

```
# Search for skills
skill_search { query: "design" }

# Add a skill
skill_add { repo: "hyperb1iss/hyperskills", skill: "tui-design" }

# List installed skills
:skill list
```

---

## How It Works

The plugin provides two handlers:

1. **`search.sh`** — Searches npm registry and GitHub for skills
   - Queries npm for packages matching the search term
   - Falls back to GitHub API for skill repos
   - Returns results sorted by relevance and download count

2. **`add.sh`** — Adds a skill to aish
   - Downloads the skill from GitHub or npm
   - Validates the skill structure (SKILL.md must exist)
   - Installs to `~/.aish/skills/` or project-scoped location
   - Emits a summary of what was installed

Both handlers are **POSIX shell scripts** with no external runtime dependencies beyond `curl` and `jq`.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `:mcp` doesn't show `npx-skills` | Plugin not discovered | Restart aish: `:restart` |
| "npm: command not found" | Node.js not installed | `brew install node` (or system package manager) |
| `skill_search` returns empty | Bad query or network issue | Try a simpler query, check internet connection |
| "curl: command not found" / "jq: command not found" | Missing system utilities | `brew install curl jq` (or system package manager) |
| "403 Forbidden" from GitHub | API rate limit or auth issue | Wait 1 hour, or set `GITHUB_TOKEN` env var |
| Skill fails to add | Malformed SKILL.md or invalid structure | Check the skill repo for `SKILL.md` at root |

---

## Next Steps

1. **Explore available skills:**
   ```
   skill_search { query: "design" }
   ```

2. **Add a skill:**
   ```
   skill_add { repo: "hyperb1iss/hyperskills", skill: "tui-design" }
   ```

3. **List installed skills:**
   ```
   :skill list
   ```

4. **Read a skill:**
   ```
   :skill add tui-design
   ```

5. **Create your own:**
   - Follow the canonical SKILL.md structure
   - Publish to a GitHub repo or npm package
   - Share with the community

---

## Creating Skills

Skills are directories containing a **`SKILL.md`** file with:

```markdown
---
name: My Skill
description: What this skill does
author: Your Name
version: 1.0.0
---

# My Skill

Instructions and workflows go here.

## Usage

```
skill_add { repo: "your-org/your-skills", skill: "my-skill" }
```

## Links

- **Plugin repo:** This directory (`~/.aish/plugins/npx-skills/`)
- **Skill repositories:**
  - https://github.com/hyperb1iss/hyperskills
  - https://github.com/LightHeart-Ventures/aish-skills
  - (Search npm for more)
- **aish docs:** https://github.com/LightHeart-Ventures/aish
