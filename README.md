# aish

An AI-native Linux shell. Type a command and it runs like any shell; type
intent and an AI agent plans, executes programs via direct `fork/exec`,
observes output, and iterates until done.

```
~/projects ❯ ll src
-rw-r--r-- 1 g g  5341 Jun  5 00:09 main.rs        ← real command: runs directly, no AI
…

~/projects ❯ who is using all my disk space
  ⚙ du -h --max-depth=1 /home/g                    ← intent: the model investigates
Mostly ~/models (412G of GGUF weights) — 87% of your usage.

~/projects ❯ vim notes.md                          ← interactive: full TTY hand-off
```

## How a line is routed

1. `:command` — REPL meta (`:help`, `:mode`, `:model`, …)
2. First word is an alias, `cd`/`exit`, or an executable in PATH → **runs
   directly** on your terminal. No model, no latency, no permission prompts.
3. Anything else — including shell machinery (`|`, `>`, `$`, globs) and English
   that merely starts with a command word (`who is grace hopper`) — goes to
   the **model**, which works through tools until done.
4. Escape hatches: `!line` forces direct execution, `?line` forces the model.

## Design

- **No shell underneath.** `run_program` execs one binary with an argv array.
  Pipes, globs, redirection don't exist — the model chains tool calls and
  filters output itself. (`aish` refuses to exec `sh`/`bash`/etc.)
- **Hybrid brain.** Claude API (`claude-opus-4-8` default) or fully-offline
  in-process inference via [mistral.rs](https://github.com/EricLBuehler/mistral.rs)
  (Qwen3-8B GGUF, lazy-loaded from the HF cache on first use).
- **Graded safety gate.** `:mode <paranoid|careful|normal|yolo>`: paranoid
  confirms every tool call, careful confirms anything not provably read-only,
  normal (default) confirms only write/create/delete — `aws s3 ls` runs free,
  `aws s3 rm` prompts — and yolo confirms nothing. MCP tools honor the
  spec's `readOnlyHint`.
- **Time-bounded execution.** Model-run programs are killed after
  `timeout_secs` (default 120) with partial output returned — a runaway
  `top -b` can't hang the session. Interactive programs (`vim`, `top`, `ssh`)
  get a full TTY hand-off instead: the user drives, the model sees the exit
  status, terminal state is restored afterwards.
- **Persistent memory.** SQLite (+ sqlite-vec) at `~/.aish/aish.db`: all
  input/output history, plus a memories table the model reads/writes through
  `remember`/`recall` tools across sessions.
- **Extensible.** MCP servers from `~/.aish/.mcp.json` (stdio transport,
  Claude-Code-compatible schema) join the tool set as `mcp__server__tool`;
  skills (Claude-convention `SKILL.md` packs) in `~/.aish/skills/` are
  advertised in the system prompt and read on demand.
- **`~/.aishrc`** — seeded from your `.bashrc` on first run; `alias`/`export`
  lines are parsed natively (no bash involved), aliases feed direct dispatch.
- **cwd is session state**, applied per-exec — `cd` is a builtin/tool that
  mutates the session, never a subprocess. Tab completion (files/dirs,
  `~`-aware, quoting names with spaces) follows it.
- **Frontend/engine split.** The REPL (rustyline) is decoupled from the engine
  (session + tools + backend) so it can graduate to a real login shell
  (`/etc/shells`, signals, job control) without rework.

## Usage

```sh
export ANTHROPIC_API_KEY=sk-ant-…
cargo run --release                 # interactive shell
cargo run --release -- -c "prompt"  # one-shot (login-shell -c style)
cargo run --release -- --backend local   # offline, in-process Qwen3-8B
cargo run --release -- --mode careful    # stricter confirmation gate
cargo build --no-default-features   # fast Claude-only build (skips mistral.rs)
```

REPL commands: `:mode <paranoid|careful|normal|yolo>` · `:model <opus|sonnet|haiku|id>` ·
`:backend <claude|local>` · `:yolo` · `:new` · `:help` · `:quit` (or Ctrl-D / `exit`).
Ctrl-C aborts the current turn — or interrupts the foreground child during a
TTY hand-off, exactly like a shell.

## Status

Working prototype. Known gaps vs a classic shell (see the gap analysis):
no pipes/redirection/`$VAR`/globs in direct dispatch (those lines route to the
model), no job control (Ctrl-Z), no script-interpreter mode, path-only tab
completion. History and memories are stored unencrypted in `~/.aish/aish.db`.
