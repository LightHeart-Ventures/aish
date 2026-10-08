# The activity ticker

Mid-turn tool activity used to be permanent scrollback. A 20-call turn left
~90–110 rows on screen that nobody reads twice, and the answer you actually
wanted was somewhere above the fold. The ticker makes that activity
**ephemeral**: the last K rows scroll in place inside a self-erasing window,
and when the turn's answer prints, the window is gone.

Ctrl-O still expands the full turn — expansion reads `session.last_turn_tools`
(state), not the screen, so nothing about it changed.

- **Setting:** `:ticker [on|off]` — bare `:ticker` reports state. Default ON.
- **Env:** `AISH_ACTIVITY_TICKER=off` (also `0`/`false`/`no`) at startup.
- **Off** restores the pre-ticker behavior: every row permanent, for forensics.

## Why K rows and not a whole-turn erase

`K = min(5, term_rows - 2)`. The clamp is the entire reason the design is safe:
the erase region can never be taller than the viewport, so the cursor-up can
never reach above the visible screen into already-scrolled conversation. A
whole-turn block erase (what `render_raw_toggle` does for its bounded block)
cannot make that guarantee for a 44-call turn.

Two rows are reserved: one for the cursor's own row below the window, one for
breathing room. On a terminal shorter than 3 rows, `rows_cap` returns 0, the
ticker reports itself inactive, and every emitter falls back to plain
`eprintln`.

## The one invariant

> **While the window is painted, nothing else may write to stderr.**

The window is addressed by a cursor-relative anchor (`CSI nF CSI 0J`). Any
foreign write moves the cursor, and the next repaint then erases rows that
belong to somebody else. That is the only visible failure mode — a torn window
mid-turn — and it is always this invariant being broken.

`teardown()` is idempotent and a no-op when nothing is painted, so the fix is
always the same: call it before you write.

## Write-discipline audit

Every stderr writer reachable mid-turn, and how it is handled. This is the
table to extend when a new mid-turn writer is added.

| Writer | Site | Handling |
|---|---|---|
| Tool row `✓ 🛠️ <desc>` | `engine.rs` `ToolSpinner::finish` | `ticker::push` |
| Output tail `… N lines — Ctrl-O` | `engine.rs` `emit_activity_stream` | `ticker::push` |
| Model narration | `engine.rs` `emit_narration` | `ticker::push_block`, falls back to `eprintln` when inactive |
| `↺ replayed <desc>` | `engine.rs` (replay path) | `ticker::push` |
| `⚠ schema-validation` | `engine.rs` `validate_output_schema` | `ticker::push` |
| Permission / confirm prompt | `engine.rs` `gated` closure | `teardown` — a prompt is a conversation; it must persist |
| TTY hand-off to a child | `tools.rs` `run_on_tty` | `teardown` — the child owns the whole terminal |
| `:raw` verbatim dump | `engine.rs` `print_raw_result` | `teardown` — unbounded output cannot live in K rows |
| Ctrl-O toggle block | `engine.rs` `render_raw_toggle` | `teardown` — two cursor anchors must never be live at once |
| Loopguard / escalation / compaction banners, final answer | after the tool batch | covered by the end-of-round `teardown` |
| Per-round `cache:` telemetry | `backend/claude.rs` | **deleted** — duplicated the live statusline, one row per API round |
| Background job live output | `jobs.rs` | no stderr writes (streams to the user's own terminal) |

The end-of-round `teardown()` (`engine.rs`, right after the tool batch) is what
makes this tractable: everything that prints *after* a round's tools — banners,
the next round's narration, the answer — lands on an anchor-free screen without
needing to know the ticker exists. Only writers that fire **inside** the tool
batch need their own call, and those are the five rows above.

## Tests

`ticker.rs` unit-tests the window as a pure transform: `push` returns the exact
byte string to write, so the escape sequences are assertable without a
terminal. Covered: growth below cap, eviction at cap, the `rows_cap` clamp at
`term_rows == 3`, teardown idempotency, and multi-row block pushes.
