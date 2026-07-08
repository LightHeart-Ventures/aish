# S6.4 / TASK-138 — Inline AI command-rewrite preview

> fish expands an abbreviation when you press space. aish expands **intent** into a
> concrete command — but turns that expansion into a *trust surface*: the model's
> candidate command is shown and must be **accepted or edited before it runs**.
> Nothing executes unconfirmed.

## Problem

aish already routes a line either to the shell (real command) or to the model
(intent), and the route-preview highlighter (TASK-132/133) colours the line so a
mis-route is visible before Enter. But when the user has *intent* ("delete every
`.tmp` under build older than a week") the only path today is a full agentic
model turn: the model decides, runs tools, and the user watches it happen. There
is no "here's the one command I'd run — okay it / tweak it / drop it" step.

This card adds that step: a low-latency, single-shot **rewrite** that turns one
line of intent into one concrete command, renders it inline in the editor, and
runs it only after the user accepts (Enter) or edits it.

## Acceptance criterion

> model rewrite renders inline; user edits/accepts; nothing runs unconfirmed

## Scope decision

The S6 sprint also covers async-on-keystroke suggestion plumbing (S6.1/TASK-135),
history ghost-text (S6.2/TASK-136), and model next-command suggestion
(S6.3/TASK-137) — none yet merged. TASK-138 is built **self-contained** so it
does not block on that plumbing:

- The rewrite is triggered **explicitly** by `:rewrite <intent>` (alias `:rw`),
  not speculatively on every keystroke. That sidesteps the cancel-in-flight
  async machinery S6.1 will add, while still delivering the trust-surface UX the
  AC asks for. When S6.1/S6.3 land, the same `rewrite::rewrite_to_command`
  primitive can be driven speculatively behind the async plumbing — this card
  deliberately leaves that seam.
- The candidate is shown by **pre-filling the line editor** (rustyline
  `readline_with_initial`) rather than inventing a new inline-overlay widget.
  That reuses the existing editor — including the route-preview highlighter,
  which paints the accepted command **green** to confirm it will run directly —
  and guarantees the "edit before run" affordance for free.

## Design

### Flow

```
~/proj ❯ :rw delete every .tmp under build older than a week
  ⚙ rewriting…
  candidate — edit, Enter to run, Ctrl-C to cancel:
~/proj ❯ find build -name '*.tmp' -mtime +7 -delete      ← prefilled, editable, green
```

1. The REPL loop recognises a `:rewrite`/`:rw` invocation (before colon
   dispatch).
2. `rewrite::rewrite_to_command` asks the active backend for ONE concrete command
   (strict system prompt, no tools, a single `complete` call — provider-agnostic,
   works on claude/grok/local).
3. The raw reply is sanitised (`sanitize_candidate`): code fences stripped, a
   leading `$ `/`% ` prompt removed, first meaningful line taken, and a literal
   `NONE` (the model's "can't be one command" sentinel) or empty reply maps to
   "no candidate".
4. On a candidate, the editor re-opens **pre-filled** with the command. The user:
   - presses **Enter** → the (possibly edited) line is re-injected at the top of
     the loop and flows through the *normal* dispatch path (so it runs exactly as
     if typed — direct for a real command, model for anything else);
   - **edits then Enter** → same, with their edits;
   - **Ctrl-C / clears the line** → nothing runs.
5. On no candidate, a dim hint points at `?<intent>` (force a full model turn).

"Nothing runs unconfirmed" holds because the candidate is never executed by the
rewrite step itself — it is only ever placed in the editor, and a human Enter is
the sole trigger.

### Re-injection (zero dispatch duplication)

The loop reads its next line from `injected.take()` when set, else from the
editor. The rewrite branch sets `injected = Some(accepted)` and `continue`s, so
the accepted command is processed by the *one* existing dispatch+model block —
no copy of that logic. A normal read is unchanged when `injected` is `None`.

### Editor seam

`LineEditor` gains `read_line_with_initial(prompt, initial)`; the rustyline impl
delegates to `readline_with_initial`. This keeps the editor abstraction (S5.1)
intact and a future reedline swap a one-method addition.

## What is pure / tested

- `rewrite::parse_invocation` — recognises the `:rewrite`/`:rw` family and
  extracts the intent (or `""` for a bare invocation → usage).
- `rewrite::sanitize_candidate` — fence stripping, prompt-sigil stripping,
  first-line extraction, `NONE`/empty → `None`.
- `rewrite::build_user_prompt` — deterministic prompt assembly (cwd + intent).

The interactive editor round-trip (prefill → Enter/edit/cancel) is the only part
that needs a TTY and is kept to a thin, obvious wiring layer.

## Out of scope (left as seams)

- Speculative/async rewrite on keystroke (S6.1).
- Caching rewrites / multi-candidate selection.
- Teaching the safety gate about an "accepted-rewrite" provenance — an accepted
  command runs through the identical gate any typed command would.
