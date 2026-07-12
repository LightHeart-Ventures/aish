# hello-world Plugin — Installation Guide

This is a **reference / smoke-test plugin** demonstrating the canonical aish plugin structure. It has **no external dependencies** and requires **no installation steps** beyond aish itself.

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
hello-world (local)
  Tools: 1
    - hello_world
```

### Try the tool

```
hello_world { name: "Alice" }
```

Expected output:

```json
{
  "message": "Hello, World! Welcome, Alice."
}
```

---

## What It Does

The hello-world plugin is a **canonical smoke-test** that proves:
- The plugin discovery system works
- MCP tools are exposed correctly
- The agent can invoke and receive tool responses

It's used in tests and as a reference for plugin authors.

---

## Next Steps

- **Study the structure:** Read `plugin.json` and the skill to understand the plugin layout
- **Build your own:** Use this as a template for new plugins
- **Report issues:** If the tool doesn't work, check aish startup logs with `RUST_LOG=debug aish`

---

## Links

- **Plugin repo:** This directory (`~/.aish/plugins/hello-world/`)
- **aish docs:** https://github.com/LightHeart-Ventures/aish
