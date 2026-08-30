# aish Plugin — Installation Guide

This plugin bundles **official aish platform skills** and reference materials for configuration, SRE troubleshooting, and operational guidance. It has **no external dependencies** and is included with aish by default.

---

## Prerequisites

**None.** This plugin is bundled with aish and requires no setup.

---

## Verification

### Check the plugin is discovered

From inside aish:

```
:mcp
```

You should see:

```
aish (local)
  Tools: (varies)
```

Or, list available skills from this plugin:

```
:skill list aish
```

You should see multiple skills listed, such as:
- `aish/webhook-broker-flyio` — Deploy aish webhook broker on Fly.io
- `aish_sre` — Site-reliability playbook for aish troubleshooting
- `aish-config-guide` — Comprehensive aish configuration guide
- And others

---

## What It Provides

The aish plugin is a **curated bundle of canonical skills** covering:

1. **Configuration & Setup**
   - `aish-config-guide` — Environment, runtime, backend, credentials, MCP servers

2. **SRE & Troubleshooting**
   - `aish_sre` — Hard-won lessons on broken releases, CI failures, OOM kills, runaway coordinators, etc.

3. **Workflow Integration**
   - `webhook-broker-flyio` — Deploy and manage webhook ingestion on Fly.io

4. **Reference Materials**
   - Release playbooks (cutting production and dev releases)
   - CI/CD troubleshooting
   - Coordinator loop budgeting and circuit breaking

---

## Loading Skills

All skills in this plugin are available via `:skill add`:

```
# Read the SRE playbook
:skill add aish_sre

# Read the configuration guide
:skill add aish-config-guide

# Deploy a webhook broker
:skill add aish/webhook-broker-flyio

# etc.
```

### Skill Categories

| Skill | Purpose | Use when… |
|-------|---------|-----------|
| `aish_sre` | SRE troubleshooting playbook | aish itself is misbehaving, a release failed, a build OOM'd, or a coordinator is stuck |
| `aish-config-guide` | Configuration reference | Setting up aish environment, MCP servers, or backend selection |
| `webhook-broker-flyio` | Deploy webhook broker | Setting up GitHub/Slack webhook ingestion on Fly.io |

---

## Next Steps

1. **Explore available skills:**
   ```
   :skill list aish
   ```

2. **Read a skill:**
   ```
   :skill add aish_sre
   ```

3. **Follow the playbook:**
   - If aish is misbehaving, follow `aish_sre`
   - If configuring aish, follow `aish-config-guide`
   - If deploying webhooks, follow `webhook-broker-flyio`

4. **Report issues:**
   - aish: https://github.com/LightHeart-Ventures/aish/issues

---

## Links

- **Plugin repo:** This directory (`~/.aish/plugins/aish/`)
- **aish docs:** https://github.com/LightHeart-Ventures/aish
- **Fly.io:** https://fly.io
