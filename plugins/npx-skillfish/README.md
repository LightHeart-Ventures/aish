# npx-skillfish

Skill source plugin that wraps the `npx skillfish` CLI for semantic skill discovery.

## Usage

```bash
:skill search skillfish:<query>
```

## How it works

This plugin provides a `skill_source` handler that delegates `:skill search` queries to the skillfish ecosystem via `npx skillfish search <string> --json`.

- Automatically discovers and lists skills from the skill.fish catalog
- Normalizes response formats into the aish `SearchResult` schema
- Degrades gracefully if `npx` or `skillfish` is unavailable
- Priority: 85 (after built-in catalog, before other sources)

## Prerequisites

- `npx` (Node.js / npm)
- `jq` (for JSON normalization)
- `skillfish` (or auto-installed via `npx --yes skillfish ...`)

## Reference

- [skill.fish](https://skill.fish)
- [Plugin design docs](../../docs/design/plugin-skill-sources.md)
