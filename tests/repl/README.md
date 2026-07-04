# aish REPL smoke tests via `agent-tty`

End-to-end smoke coverage that drives the **real aish REPL** through
[coder/agent-tty](https://github.com/coder/agent-tty) and asserts on what the
terminal actually renders.

## Why agent-tty

aish is a full-screen **alt-screen TUI REPL** (ratatui: boot banner, bottom
status bar, editable input line) — *not* a line-oriented POSIX shell. That rules
out naive `expect`/pipe scraping. `agent-tty` gives us:

- an **isolated terminal host** (`--home <dir>`) so runs never touch your real
  agent-tty registry;
- a real **PTY** with a **semantic screen renderer** (`libghostty-vt`), so we
  gate on rendered state (`wait --text`, `--screen-stable-ms`, `--exit`) instead
  of blind `sleep`s;
- **machine-readable `--json`** envelopes with stable exit codes, so assertions
  scrape structured output, not raw bytes;
- artifacts: **asciicast** recordings (always) and **PNG screenshots**
  (best-effort — needs Playwright chromium).

`tests/pty_harness.rs` already covers the kernel job-control invariants at the
syscall level. This harness is the complementary layer: the **end-user REPL as
it renders on a terminal**.

## What the smoke asserts (`agent_tty_smoke.sh`)

1. **Boot** — the `AI-native shell` banner + version string + `❯` prompt glyph
   render within 20 s.
2. **`:help`** — a built-in command applied via `type`+`Enter` renders the help
   body (`:quit`, `Ctrl-O` bindings).
3. **Artifacts** — exports `artifacts/aish_repl_smoke.cast` (+ `.png` when
   chromium is installed).
4. **Clean exit** — `:quit` actually terminates the process (`wait --exit`).

It is **hermetic**: only aish built-in `:commands` are used, so **no model /
API call** is made and **no `ANTHROPIC_API_KEY` is required**. Safe offline & in
CI.

> aish is not bash — the harness drives it with agent-tty `type` +
> `send-keys ["Enter"]` (literal interactive typing), **never** `run` (whose
> hidden shell completion-marker assumes a POSIX shell and corrupts the TUI).

## Run it

```bash
make test-repl
# or directly:
tests/repl/agent_tty_smoke.sh
```

### Prerequisites

| Need            | Notes                                                            |
|-----------------|-----------------------------------------------------------------|
| Node `>=24 <27` | agent-tty engine requirement; run via `npx agent-tty@0.5.0`.    |
| `jq`            | parses the `--json` envelopes.                                  |
| an aish binary  | `make build-fast` (or set `AISH_BIN`).                          |
| chromium (opt.) | `npx playwright install chromium` to also capture a PNG.       |

### Env knobs

| Var                 | Default                | Purpose                                             |
|---------------------|------------------------|-----------------------------------------------------|
| `AISH_BIN`          | auto-detect¹           | path to the aish binary under test.                 |
| `AGENT_TTY_VERSION` | `0.5.0`                | npm version run via `npx`.                          |
| `ARTIFACT_DIR`      | `tests/repl/artifacts` | where `.cast`/`.png` land.                          |
| `AISH_REPL_STRICT`  | `0`                    | `1` → a missing prerequisite is a hard failure, not a SKIP. |
| `DEBUG`             | `0`                    | `1` → xtrace every agent-tty call.                  |

¹ auto-detect order: `$AISH_BIN` → `target/release/aish` → `target/debug/aish` →
`command -v aish`.

## Behaviour when prerequisites are absent

By default the script **SKIPs cleanly (exit 0)** if Node/jq/agent-tty or a built
binary is missing — so it never blocks a contributor who just wants
`cargo test`. CI sets `AISH_REPL_STRICT=1` so those same gaps become hard
failures in the dedicated `repl-smoke` workflow.

## Extending

Add a new observable check by driving more built-ins and asserting on the
snapshot, e.g.:

```bash
AT batch "$SID" '[{"type":":workers"},{"sendKeys":["Enter"]},{"wait":{"screenStableMs":1000,"timeout":8000}}]' --json
assert_contains "$(snapshot_text)" 'No background' ":workers renders empty state"
```

Keep additions **hermetic** (built-ins only) unless you deliberately gate a
model-backed case behind a real `ANTHROPIC_API_KEY` and mark it non-CI.
