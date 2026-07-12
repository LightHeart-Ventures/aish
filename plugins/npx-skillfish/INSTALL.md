# npx-skillfish Plugin — Installation Guide

This plugin enables **skill.fish integration** in aish, allowing you to discover, import, and manage skills from the **agentskills.io / skillfish ecosystem** directly within the aish REPL.

---

## Prerequisites

### Node.js / npx

The plugin uses **`npx`** to run the `skillfish` CLI. You'll need:

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

**That's it.** The plugin auto-downloads `skillfish` on first use via `npx`.

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

You should see a tool like:

```
npx-skillfish (local)
  Tools: 1
    - skillfish_search
```

Or check directly:

```sh
ls ~/.aish/plugins/npx-skillfish/
# Should show: plugin.json, README.md, skills/
```

### Step 3: Try importing a skill

From inside aish:

```
skillfish_search { repo: "hyperb1iss/hyperskills" }
```

Expected output: A list of available skills from the repository.

---

## Installation Summary

| Step | Command | Expected Output |
|------|---------|-----------------|
| 1. Install Node.js | `brew install node` (or system package manager) | `npm --version` prints version |
| 2. Check aish plugin | `:mcp` from inside aish | `npx-skillfish` appears in MCP server list |
| 3. Test a search | `skillfish_search { repo: "..." }` | Skill list returned |

---

## Configuration

### Environment Variables

```sh
# Optional: set default skillfish registry
export SKILLFISH_REGISTRY="https://skill.fish"  # Default: https://api.skill.fish
```

### Command Usage

From inside aish:

```
# Search for skills
skillfish_search { repo: "hyperb1iss/hyperskills", query: "design" }

# Import a skill
:skill add hyperb1iss/hyperskills/tui-design

# List imported skills
:skill list
```

---

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `:mcp` doesn't show `npx-skillfish` | Plugin not discovered | Restart aish: `:restart` |
| "npm: command not found" | Node.js not installed | `brew install node` (or system package manager) |
| `skillfish_search` returns empty | Bad repo name or network issue | Check repo exists at https://github.com/owner/repo, check internet |
| "npx: ERR!" | npm registry timeout or package missing | Retry in a minute, or run `npm install -g skillfish` manually |

---

## Next Steps

1. **Explore the skillfish ecosystem:**
   ```
   skillfish_search { repo: "hyperb1iss/hyperskills" }
   ```

2. **Import a skill:**
   ```
   :skill add hyperb1iss/hyperskills/tui-design
   ```

3. **List installed skills:**
   ```
   :skill list
   ```

4. **Read the skillfish docs:**
   - https://skill.fish
   - https://github.com/hyperb1iss/hyperskills

---

## Links

- **Plugin repo:** This directory (`~/.aish/plugins/npx-skillfish/`)
- **skillfish:** https://skill.fish
- **Hyperskills repo:** https://github.com/hyperb1iss/hyperskills
- **aish docs:** https://github.com/LightHeart-Ventures/aish
