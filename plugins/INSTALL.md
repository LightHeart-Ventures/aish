# aish Plugin Registry — Installation Guide Index

This directory (`plugins/`) contains all **aish plugins** — MCP servers, webhook handlers, and local tools that extend aish's capabilities. Each plugin has its own **INSTALL.md** guide with step-by-step setup instructions, prerequisites, and troubleshooting.

---

## Quick Start

For each plugin, read its **`INSTALL.md`** file. All guides follow the same structure:

1. **Prerequisites** — What you need to install first (external binaries, API keys, env vars)
2. **Verification** — Step-by-step checks to confirm installation
3. **Troubleshooting** — Common issues and fixes
4. **Configuration** — Optional customization
5. **Usage** — Examples and next steps

---

## Plugin Directory

| Plugin | Type | Depends On | Use When… |
|--------|------|-----------|-----------|
| **aish** | Local Skills | None | Reading SRE playbooks, config guides, alert patterns |
| **ccquota** | Local (MCP) | Anthropic API | Monitoring Claude API spend and quota |
| **codebase-memory** | MCP Server | DeusData/codebase-memory-mcp binary | Graph-based code search instead of grep |
| **github** | Webhook Handlers | GitHub webhooks, (optional) gh CLI | Ingesting GitHub PR/workflow/release events |
| **hello-world** | Local (Reference) | None | Testing plugin discovery, reference implementation |
| **npx-skills** | Local (Shell) | Node.js/npx | Searching npm registry for skills |
| **npx-skillfish** | Local (Shell) | Node.js/npx | Importing skills from agentskills.io/skillfish |
| **signoz-observability** | MCP Server | SigNoz instance, API key | Querying logs, traces, metrics, alerts |

---

## Plugin Guides

### By Category

#### Core Platform Skills
- **[aish/INSTALL.md](./aish/INSTALL.md)** — Official aish platform skills (SRE, config, webhooks, alerts)

#### API Quotas & Spend
- **[ccquota/INSTALL.md](./ccquota/INSTALL.md)** — Claude API consumption tracking

#### Code Intelligence
- **[codebase-memory/INSTALL.md](./codebase-memory/INSTALL.md)** — Graph-based code search and analysis

#### GitHub Integration
- **[github/INSTALL.md](./github/INSTALL.md)** — Webhook handlers for GitHub events

#### Skill Management
- **[npx-skills/INSTALL.md](./npx-skills/INSTALL.md)** — Search and install skills from npm
- **[npx-skillfish/INSTALL.md](./npx-skillfish/INSTALL.md)** — Import skills from agentskills.io/skillfish

#### Observability
- **[signoz-observability/INSTALL.md](./signoz-observability/INSTALL.md)** — Logs, traces, metrics, alerts via SigNoz

#### Reference / Testing
- **[hello-world/INSTALL.md](./hello-world/INSTALL.md)** — Canonical smoke-test plugin

---

## Installation Paths

### Path 1: MCP Server Plugins (Binary Dependencies)

These plugins require an **external binary** to run:

1. **codebase-memory**
   - Needs: `codebase-memory-mcp` binary
   - Install via: Homebrew, aish command, manual download, or source build
   - [Install guide](./codebase-memory/INSTALL.md)

2. **signoz-observability**
   - Needs: `signoz-observability-mcp` binary, SigNoz instance, API key
   - Install via: `:plugin install`, manual download, or source build
   - [Install guide](./signoz-observability/INSTALL.md)

### Path 2: Local Tools (Script-Based)

These plugins run **shell scripts** with no binary dependencies:

1. **aish**
   - Bundled platform skills (no installation needed)
   - [Install guide](./aish/INSTALL.md)

2. **ccquota**
   - Local quota tracking tool
   - [Install guide](./ccquota/INSTALL.md)

3. **github**
   - Webhook handlers (shell scripts)
   - Needs: Webhook broker (Fly.io recommended)
   - [Install guide](./github/INSTALL.md)

4. **npx-skills** & **npx-skillfish**
   - Skill search and management via npm/GitHub
   - Needs: Node.js / npx (install from https://nodejs.org/)
   - [npx-skills guide](./npx-skills/INSTALL.md) | [npx-skillfish guide](./npx-skillfish/INSTALL.md)

5. **hello-world**
   - Reference plugin (bundled, no installation)
   - [Install guide](./hello-world/INSTALL.md)

---

## Verification Checklist

Once you've installed a plugin, verify it's working:

```sh
# From inside aish REPL

# List all enrolled MCP servers
:mcp

# Look for your plugin in the list
# You should see the plugin name + tool count

# Test a query
# (each plugin's INSTALL.md has specific test examples)
```

---

## Common Tasks

### I want to query logs/traces/metrics

**→ Install [signoz-observability](./signoz-observability/INSTALL.md)**

Prerequisites: SigNoz instance (cloud or self-hosted), API key

### I want to search code by definition/usage

**→ Install [codebase-memory](./codebase-memory/INSTALL.md)**

Prerequisites: `codebase-memory-mcp` binary (Homebrew or manual download)

### I want to ingest GitHub webhooks

**→ Install [github](./github/INSTALL.md)**

Prerequisites: Webhook broker (recommended: Fly.io), optional: `gh` CLI for auto-fix

### I want to monitor Claude API spend

**→ Install [ccquota](./ccquota/INSTALL.md)**

Prerequisites: None (uses existing `ANTHROPIC_API_KEY`)

### I want to search for and import skills

**→ Install [npx-skills](./npx-skills/INSTALL.md) and/or [npx-skillfish](./npx-skillfish/INSTALL.md)**

Prerequisites: Node.js / npm

### I want to test the plugin system

**→ Verify [hello-world](./hello-world/INSTALL.md)**

Prerequisites: None (bundled with aish)

---

## Troubleshooting

### Plugin not discovered (`:mcp` doesn't show it)

1. **Restart aish:** `:restart` (or exit and re-run `aish`)
2. **Check binary on PATH:** `which <plugin-binary-name>`
3. **Check env vars:** `echo $REQUIRED_ENV_VAR`
4. **Check plugin.json:** Ensure it exists in the plugin directory
5. **Check logs:** `RUST_LOG=debug aish` to see startup errors

### Tool invocation fails

1. **Read the plugin's INSTALL.md troubleshooting table**
2. **Verify prerequisites are installed** (binary, API key, env vars)
3. **Test a simple query** (examples in each INSTALL.md)
4. **Check upstream project** (linked in each INSTALL.md)

### Can't find a plugin

1. **Check it exists:** `ls ~/.aish/plugins/<plugin-name>/`
2. **Check repo location:** `ls ~/projects/aish/plugins/<plugin-name>/`
3. **Reinstall:** Follow the plugin's INSTALL.md from scratch

---

## Contributing

### Adding a New Plugin

1. Create a new directory: `plugins/<plugin-name>/`
2. Add `plugin.json` (define tools/commands)
3. Add `INSTALL.md` using the template from [`TEMPLATE_INSTALL.md`](./TEMPLATE_INSTALL.md) (if it exists, otherwise use the structure from an existing plugin)
4. Add tool implementations (shell scripts, MCP server binary, etc.)
5. Test with `:mcp` from inside aish
6. Submit a PR

### Updating Plugin Documentation

Each INSTALL.md should:
- List **all prerequisites** (binaries, API keys, env vars)
- Provide **multiple installation options** (package manager, manual, source build)
- Include **step-by-step verification**
- Have a **troubleshooting table** with common issues
- Link to **upstream projects**

See existing INSTALL.md files for examples.

---

## Links

- **aish repo:** https://github.com/LightHeart-Ventures/aish
- **aish docs:** https://github.com/LightHeart-Ventures/aish/tree/main/docs
- **Issue tracker:** https://github.com/LightHeart-Ventures/aish/issues
- **Discussions:** https://github.com/LightHeart-Ventures/aish/discussions
