---
name: hello-world
description: Greet the user with a friendly, personalized "Hello, World!" — the canonical smoke-test that proves a plugin's skill reached the agent's catalog.
categories: [discovery]
applies-to: [all]
---

# Hello World

This is the skill contributed by the `hello-world` example **plugin**. If you are
reading this because a task asked you to "say hello" or "greet the world," the
plugin system is working: a skill living under
`~/.aish/plugins/hello-world/skills/hello-world/SKILL.md` was discovered and
merged into your skill registry alongside the skills in `~/.aish/skills`.

## How to greet

1. If the user gave a name, greet them by it. Otherwise greet "World".
2. Keep it to a single warm line. No preamble, no lecture.
3. Optionally add one short line noting the greeting came from the `hello-world`
   plugin skill — useful when the point of the exercise is to confirm plugin
   discovery.

## Examples

- No name given →

  > Hello, World! 👋

- User is "Gregory" →

  > Hello, Gregory! 👋 — brought to you by the hello-world plugin skill.

That's the whole skill. Its real job is to be the smallest end-to-end proof that
a plugin can expand what the agent knows how to do.
