# hello-world — example aish plugin

The smallest useful aish plugin. It contributes **one skill** (`hello-world`) and
nothing else — no MCP server, no tools, no webhooks. Its only job is to be a
runnable proof that aish's plugin discovery finds a plugin on disk and that a
plugin can **expand the skill registry** the agent sees.

## Layout

```
hello-world/
├── plugin.json                       # manifest (id, name, version, enabled)
└── skills/
    └── hello-world/
        └── SKILL.md                  # the greeting skill (Claude-skill convention)
```

A plugin is any directory under `~/.aish/plugins/` that contains a readable
`plugin.json`. Its skills use the same `skills/<name>/SKILL.md` layout as
`~/.aish/skills/`, so they load through the exact same parser.

## Try it

Copy the plugin into your aish config home and start aish:

```
mkdir -p ~/.aish/plugins
cp -r examples/plugins/hello-world ~/.aish/plugins/hello-world
aish
```

On startup aish scans `~/.aish/plugins/*/plugin.json`, loads each enabled
plugin's skills, and merges them into the catalog advertised in the system
prompt. Ask the agent to *"say hello world"* and it will read this plugin's
`SKILL.md` and greet you.

Confirm it's loaded from the interactive shell:

```
:skill list          # installed (~/.aish/skills) skills
```

(Plugin-contributed skills show up in the agent's catalog / system prompt; the
`hello-world` skill's description will mention it came from the plugin.)

## Manifest fields

| field         | required | meaning                                             |
| ------------- | -------- | --------------------------------------------------- |
| `id`          | yes      | stable plugin identifier (`hello-world`)            |
| `name`        | no       | human-facing name (defaults to `id`)                |
| `version`     | no       | semver string                                       |
| `description` | no       | one-line summary                                    |
| `enabled`     | no       | set `false` to keep the plugin on disk but inert    |

Unknown fields are ignored, so this manifest stays forward-compatible with the
richer plugin schema in [`docs/PLUGIN_SYSTEM_DESIGN.md`](../../../docs/PLUGIN_SYSTEM_DESIGN.md)
(MCP servers, tools, webhooks, hooks, memory, schemas) as those phases land.
