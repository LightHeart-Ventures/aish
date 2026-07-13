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
- `aish-sre` — Site-reliability playbook for aish troubleshooting
- `aish-config-guide` — Comprehensive aish configuration guide
- And others

---

## What It Provides

The aish plugin is a **curated bundle of canonical skills** covering:

1. **Configuration & Setup**
   - `aish-config-guide` — Environment, runtime, backend, credentials, MCP servers
   - `claude-oauth-toggle` — Switch between two CLAUDE_CODE_OAUTH_TOKEN entries

2. **SRE & Troubleshooting**
   - `aish-sre` — Hard-won lessons on broken releases, CI failures, OOM kills, runaway coordinators, etc.
   - `aish_sre` — Complementary troubleshooting playbook

3. **Observability & Automation**
   - `alert-batch-composition` — Compose multi-condition alert groups
   - `alert-condition-validator` — Pre-flight syntax check for alert conditions
   - `alert-native-probe-builder` — Convert semantic patterns to native file/command probes

4. **Workflow Integration**
   - `webhook-broker-flyio` — Deploy and manage webhook ingestion on Fly.io

5. **Reference Materials**
   - Release playbooks (cutting production and dev releases)
   - CI/CD troubleshooting
   - Coordinator loop budgeting and circuit breaking

---

## Loading Skills

All skills in this plugin are available via `:skill add`:

```
# Read the SRE playbook
:skill add aish-sre

# Read the configuration guide
:skill add aish-config-guide

# Deploy a webhook broker
:skill add aish/webhook-broker-flyio

# etc.
```

### Skill Categories

| Skill | Purpose | Use when… |
|-------|---------|-----------|
| `aish-sre` | SRE troubleshooting playbook | aish itself is misbehaving, a release failed, a build OOM'd, or a coordinator is stuck |
| `aish-config-guide` | Configuration reference | Setting up aish environment, MCP servers, or backend selection |
| `alert-batch-composition` | Multi-condition alerts | Monitoring interdependent signals (OR/AND logic) |
| `alert-condition-validator` | Alert pre-flight checks | Validating alert syntax before arming |
| `alert-native-probe-builder` | Convert patterns to probes | Reducing coordinator overhead with native file/command probes |
| `webhook-broker-flyio` | Deploy webhook broker | Setting up GitHub/Slack webhook ingestion on Fly.io |

---

## Next Steps

1. **Explore available skills:**
   ```
   :skill list aish
   ```

2. **Read a skill:**
   ```
   :skill add aish-sre
   ```

3. **Follow the playbook:**
   - If aish is misbehaving, follow `aish-sre`
   - If configuring aish, follow `aish-config-guide`
   - If setting up alerts, follow `alert-*` skills
   - If deploying webhooks, follow `webhook-broker-flyio`

4. **Report issues:**
   - aish: https://github.com/LightHeart-Ventures/aish/issues

---

## Links

- **Plugin repo:** This directory (`~/.aish/plugins/aish/`)
- **aish docs:** https://github.com/LightHeart-Ventures/aish
- **Fly.io:** https://fly.io
