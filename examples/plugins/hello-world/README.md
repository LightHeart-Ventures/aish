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
| `config_schema` | no     | JSON-Schema-shaped config declaration (Phase 1.4)   |

## Configuration (Phase 1.4)

When a plugin declares a `config_schema`, aish resolves the plugin's config on
discovery: it reads `config.json` (optional), fills any missing keys from the
schema's `default`s, expands every `${env:VAR}` reference against the process
environment, then validates `required` keys and declared `type`s. Secrets live
only in the environment — `config.json`/`plugin.json` carry `${env:VAR}`
references, never the resolved value.

This example ships both halves:

```jsonc
// plugin.json → config_schema.properties
"greeting": { "type": "string",  "default": "Hello, World!" },
"shout":    { "type": "boolean", "default": false },
"greeter":  { "type": "string",  "default": "${env:USER}" }   // env-ref default

// config.json (overrides the schema defaults)
{ "greeting": "¡Hola, mundo!", "shout": true }
```

Resolved config (with `USER=ada`): `greeting="¡Hola, mundo!"`, `shout=true`,
`greeter="ada"`. A config error (unset `${env:VAR}`, missing `required` key,
type mismatch) never drops the plugin — its skills still load; only the resolved
`config` is withheld.

Unknown fields are ignored, so this manifest stays forward-compatible with the
richer plugin schema documented under [`docs/reference/plugins/`](../../../docs/reference/plugins/)
(MCP servers, tools, webhooks, hooks, memory, schemas) as those phases land.
