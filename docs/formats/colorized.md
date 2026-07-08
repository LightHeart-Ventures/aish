# Colorized + Emoji Output

aish paints status output with color and emoji so a glance tells you what's
happening — which background workers are running, which finished, which failed —
without reading every word. This document is the single source of truth for the
scheme. The implementation lives in [`src/style.rs`](../src/style.rs); the
flagship consumer is the `:workers` command in `src/repl.rs`.

## Goals

- **Scannable status tables.** Color + an emoji per row turns a wall of text
  into something the eye triages instantly (green = good, red = needs you).
- **One scheme, one switch.** Every styled surface routes through `style.rs`, so
  the palette stays consistent and `--no-color` turns *all* of it off.
- **Never break piped output.** ANSI escapes only ever reach a real terminal;
  redirect or pipe `aish` and you get clean, plain text.

## When color is emitted

`style::colors_enabled()` is the gate. Color is **off** when any of these holds:

| Condition | Why |
|---|---|
| `--no-color` flag was passed | Explicit user opt-out (`style::set_no_color`) |
| `NO_COLOR` env var is set (any value) | The [no-color.org](https://no-color.org) convention |
| stdout is not a TTY (piped / redirected) | Escape codes must not land in files or downstream tools |

Otherwise color is on. Because the gate is checked at format time, the *same*
code path produces colored output at the prompt and plain output through a pipe —
no separate "plain" rendering branch to drift.

## Color scheme

Status drives the color. The mapping is in `style::classify_status`:

| Status (and synonyms) | Color | Emoji | Meaning |
|---|---|---|---|
| `done`, `success`, `complete`, `merged`, `ok` | 🟢 green | ✅ | Finished successfully |
| `failed`, `error`, `aborted`, `timeout`, `cancelled` | 🔴 red | ❌ | Needs attention |
| `running`, `working`, `in_progress`, `active` | 🟡 yellow | 🔄 | In flight right now |
| `queued`, `pending`, `dispatched`, `waiting` | 🔵 blue | ⏳ | Not started yet |
| free-form coordinator phase (`planning`, `reviewing`, `pushing`, `building`, `testing`, …) | 🟡 yellow | 🔄 | Active work (matched by substring) |
| anything else | dim | • | Neutral / unknown |

Result cells (`style::styled_result`) are colored by their leading glyph,
matching the success/failure summaries the worker layer already produces:

| Cell shape | Color |
|---|---|
| `✓ …` / `✅ …` | green |
| `✗ …` / `❌ …` | red |
| `—` / empty | dim |

`style::dim()` is the shorthand for secondary text — footnotes and hints below a
table (`* = launched from this session`, etc.).

## Emoji usage

Two axes carry emoji:

- **Status indicator** — the leading glyph on every status cell (table above).
  Chosen for instant traffic-light reading: ✅ good, ❌ bad, 🔄 busy, ⏳ waiting.
- **Job type** — `style::job_type_emoji`, prefixing the id column so a mixed
  listing tells worker from batch from goal at a glance:

  | Job kind | Emoji |
  |---|---|
  | worker / coordinator | 🤖 |
  | batch | 📦 |
  | goal | 🎯 |
  | other | • |

Emoji are placed at the *start* of a cell, never mid-text, so column alignment is
predictable. `src/md.rs` measures display width with `unicode-width` (and strips
ANSI first), so wide glyphs and embedded color codes never misalign the rendered
table.

## `:workers` — the showcase

`:workers` lists this session's background coordinators (`:workers all` widens to
every session). Each row now reads as:

```
| Worker      | Session | Status        | Doing            | Result   |
|-------------|---------|---------------|------------------|----------|
| 🤖 w_a7k3m2 | mysess* | 🔄 running    | refactor parser  | —        |
| 🤖 w_x1p9qz | mysess* | ✅ done       | add CI cache     | ✓ #142   |
| 🤖 orc_55ff | other   | ❌ failed     | flaky test fix   | ✗ timeout|
```

- The **Worker** column is prefixed with 🤖 so the rows scan as a fleet of agents.
- The **Status** column carries the emoji + a color-matched label (yellow while
  running, green when done, red on failure) — the fastest "is everything OK?"
  signal.
- The **Result** column is green for a success / PR link, red for a failure
  reason, dim when there's nothing yet.

Piped (`aish -c ':workers' | cat`) or with `--no-color`, the very same rows come
out as plain `🤖 w_a7k3m2 | mysess* | running | …` — emoji are kept (they are
plain text), ANSI color is dropped.

## Testing

`style.rs` ships pure `_with(…, color_on: bool)` variants of every formatter so
the color/emoji mapping is unit-tested without a TTY: `classify_status` buckets,
`paint` on/off, `styled_status`/`styled_result` shapes, the `--no-color`
override, and `job_type_emoji`. See the `#[cfg(test)] mod tests` at the bottom of
the file.

## Extending the scheme

Add a status synonym or phase keyword in `classify_status`; add a job kind in
`job_type_emoji`. Keep new colors inside the `Color` enum so the one palette
stays authoritative, and add a case to the tests. Don't hand-write `\x1b[…m`
escapes at call sites — route through `style::paint` so `--no-color` and the
pipe-detection keep working.
