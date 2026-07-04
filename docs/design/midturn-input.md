# Mid-turn input: type at a prompt while a turn is running

## Goal

While a model turn is in flight (thinking / mid tool-call), keep the input
prompt visible **one blank line below the streaming turn output**, let the
operator type into it, submit with Enter, and keep **Shift-Tab** working (cycle
the attach cursor / worker view). Today the operator can only type at the idle
rustyline prompt between turns; during a turn the reader thread consumes bytes
solely to detect Shift-Tab.

## Status

| Piece | State |
|---|---|
| `KeyParser` — cbreak bytes → semantic `Key`s (UTF-8 + CSI fragmentation, Shift-Tab superset of `scan_csi_z`) | ✅ landed, unit-tested (`src/midturn_input.rs`) |
| `LineBuf` — UTF-8 single-line editor (insert/move/kill, cursor math) | ✅ landed, unit-tested |
| `Action` — `Submit(line)` / `CycleWorker` / `None` fold outcome | ✅ landed, unit-tested |
| `FooterRender` — pure ANSI draw/erase math (blank-gap + wrap-aware erase) | ✅ landed, unit-tested |
| Type-ahead queue (reader → REPL handoff) | ⬜ wiring |
| Engine-write serialization + live footer paint | ⬜ wiring (needs interactive TTY to verify) |

The **pure core is complete and testable without a terminal**. The remaining
work is tty wiring that must be validated in an interactive session — it cannot
be exercised from a headless/background coordinator (no controllable real TTY).

## Architecture

### The hard constraint: one cursor, two writers

During a turn there is **no single output funnel**. Turn output is a mix of:

- the async thinking / tool spinner (carriage-return + erase animation on the
  main task), and
- scattered `eprintln!` / `println!` narration and tool `✓/✗` result lines.

Meanwhile the **keywatch reader thread** owns raw byte reads in cbreak. If that
reader thread also painted a footer at the tty cursor, its writes would race the
spinner/narration writes to the same cursor → corruption.

**Therefore a pinned footer requires serializing every turn-time write against
the footer repaint through one shared lock.** The invariant for any write `W`
emitted during a turn is:

```
lock →  erase_footer()  →  W  →  draw_footer()  → unlock
```

and the reader thread, on each decoded key, does:

```
lock →  erase_footer()  →  draw_footer()  → unlock     // redraw with new text
```

`FooterRender::draw()` / `::erase()` are the exact byte strings for those two
primitives and are strict inverses (see the cursor contract in the rustdoc).

### Shared footer handle

```rust
struct Footer {
    prompt: &'static str,   // e.g. "» "
    line: LineBuf,          // current edit state
    cols: usize,            // from terminal width, refreshed on SIGWINCH
    shown: bool,            // is a footer currently painted?
}
type FooterHandle = Arc<Mutex<Footer>>;
```

- Created when a turn starts; dropped/hidden when it ends (before returning to
  the rustyline prompt, which must start on a clean line).
- The engine's write sites take a `&FooterHandle` and wrap their writes with the
  erase→write→draw dance above. A single helper —
  `footer_print(&FooterHandle, args)` — centralizes it so call sites stay small.
- The keywatch reader thread holds a clone; on each `Key` it folds via
  `LineBuf::apply` and repaints under the lock.

### Engine write sites to route through `footer_print`

These are the turn-time emitters that must go through the lock (grep targets):

- the thinking / tool spinner draw+clear,
- `emit_narration` (assistant narration lines),
- tool-start and tool-finish (`✓` / `✗`) lines,
- streamed assistant text tokens (if/when echoed).

Idle-time output (between turns, at the rustyline prompt) is unaffected — the
footer only exists during a turn.

### Submit path: type-ahead, not injection

A line submitted mid-turn is **queued**, not injected into the in-flight request
(that would malform the model stream). Reuse the REPL's existing
`injected: Option<String>` mechanism:

```rust
// reader thread, on Action::Submit(line):
type_ahead.lock().push_back(line);      // Arc<Mutex<VecDeque<String>>>

// REPL loop, immediately after a turn returns and before reading the next
// rustyline line:
if let Some(next) = type_ahead.lock().pop_front() {
    injected = Some(next);              // runs as the next command
}
```

`Action::CycleWorker` (Shift-Tab) calls the **existing** worker-cycle handler —
behaviour is preserved exactly, now driven through the unified `KeyParser`
(`ESC [ Z` ⇒ `Key::ShiftTab`), which is a strict superset of the old
`scan_csi_z`.

## Testing

- Pure core: `cargo test --no-default-features midturn_input::` — parser
  (UTF-8/CSI fragmentation, Shift-Tab, arrows, ctrl-keys), `LineBuf` editing, and
  `FooterRender` draw/erase inverse + wrap math.
- Wiring: manual interactive verification in a real terminal — type during a
  long tool call, confirm the prompt stays one blank line below streaming
  output, Enter queues + runs the line next, Shift-Tab still cycles workers, and
  the terminal is clean on return to the idle prompt. **This step needs a human
  at a TTY; it is out of scope for headless/background runs.**

## Follow-ups (deferred)

- Visualize the in-text cursor position (v1 keeps the terminal caret at end of
  text; `LineBuf::cursor()` already tracks the real index for a later
  `\x1b[{n}D` after `draw`).
- Up/Down history recall (keys are decoded and currently no-op in `LineBuf`).
