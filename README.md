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

## Platform Support

**Supported:**
- **macOS** 12+ (x86_64, arm64/Apple Silicon)
- **Linux** (glibc 2.35+): Ubuntu 24.04 LTS, Ubuntu 20.04 LTS, Debian 12, Fedora 38+, etc.
  - **Ubuntu 24.04 LTS**: [Detailed install guide](UBUNTU_24.04_INSTALL.md)
- **WSL** (Windows Subsystem for Linux) via Ubuntu/Debian base

**Not supported:**
- Windows native (use WSL instead)
- iOS/Android
- FreeBSD (possible but untested)

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
  (Qwen3-1.7B GGUF default, lazy-loaded from the HF cache on first use with a
  download progress bar; swap via `AISH_LOCAL_MODEL_ID` — no rebuild).
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
- **Last-output addressing.** The previous output is addressable on the next
  line. In direct dispatch, `$LAST` (alias `$_`) expands to the most recent
  recorded output — `grep ERROR $LAST`, `echo $LAST`. For the model it's
  automatic: after a command, `summarize that` references the prior output
  without re-running it. Large outputs are truncated head-first to 4000 bytes
  with an `…[truncated]` marker. (Output is read from the SQLite `history`
  table; streamed interactive programs — `vim`, `top` — aren't captured.)
- **Extensible.** MCP servers from `~/.aish/.mcp.json` (stdio transport,
  Claude-Code-compatible schema) join the tool set as `mcp__server__tool`;
  skills (Claude-convention `SKILL.md` packs) in `~/.aish/skills/` are
  advertised in the system prompt and read on demand.
- **`~/.aishrc`** — seeded from your `.bashrc` on first run; `alias`/`export`
  lines are parsed natively (no bash involved), aliases feed direct dispatch.
- **cwd is session state**, applied per-exec — `cd` is a builtin/tool that
  mutates the session, never a subprocess. Tab completion (files/dirs,
  `~`-aware, quoting names with spaces) follows it.
- **History ghost text.** As you type, the most recent matching command from
  history is previewed inline as gray fish-style ghost text; press `→` or
  `Ctrl-F` at end-of-line to accept it. Purely visual until accepted — Enter
  still runs only what you typed.
- **Frontend/engine split.** The REPL (rustyline) is decoupled from the engine
  (session + tools + backend) so it can graduate to a real login shell
  (`/etc/shells`, signals, job control) without rework.

## Installation

### One-Command Ubuntu Install

Pick the script matching your release — both install dependencies, build aish,
and register it in `/etc/shells`.

**Ubuntu 24.04 LTS (Noble Numbat)**
```sh
# From main (recommended once CDN syncs)
curl -sSL https://raw.githubusercontent.com/LightHeart-Ventures/aish/main/install-ubuntu-24.04.sh | bash

# Or from a repo clone (works immediately)
git clone https://github.com/LightHeart-Ventures/aish.git
cd aish && bash install-ubuntu-24.04.sh
```

### Quick Start (All Platforms)

```sh
# 1. Clone the repo
git clone https://github.com/LightHeart-Ventures/aish.git
cd aish

# 2. Install Rust (if needed)
rustup update stable

# 3. Build and install
make install

# 4. Set your API key
export ANTHROPIC_API_KEY=sk-ant-…

# 5. Launch
aish
```

**Ubuntu LTS users**: See the [detailed install guide](UBUNTU_24.04_INSTALL.md) for prerequisites, troubleshooting, and advanced configuration.

### Build from Source

```sh
export ANTHROPIC_API_KEY=sk-ant-…
cargo run --release                 # interactive shell
cargo run --release -- -c "prompt"  # one-shot (login-shell -c style)
cargo run --release -- script.aish  # run a script file, then exit
cargo run --release -- --backend local   # offline, in-process Qwen3-1.7B
cargo run --release -- --mode careful    # stricter confirmation gate
cargo build --no-default-features   # fast Claude-only build (skips mistral.rs)
```

Local model selection (no rebuild): `AISH_LOCAL_MODEL_ID` picks the GGUF repo
(default `Qwen/Qwen3-1.7B-GGUF`, which ships a single `Q8_0` quant); for other
repos the tokenizer and `*-Q4_K_M.gguf` filename are derived from the Qwen
naming convention, or set explicitly with `AISH_LOCAL_TOK_ID` /
`AISH_LOCAL_MODEL_FILE`:

```sh
AISH_LOCAL_MODEL_ID=Qwen/Qwen3-4B-GGUF cargo run --release -- --backend local
```

REPL commands: `:mode <paranoid|careful|normal|yolo>` · `:model <opus|sonnet|haiku|id>` ·
`:backend <claude|local>` · `:yolo` · `:new` · `:help` · `:quit` (or Ctrl-D / `exit`).
Ctrl-C aborts the current turn — or interrupts the foreground child during a
TTY hand-off, exactly like a shell. `→`/`Ctrl-F` accept the history
autosuggestion.

## Scripting

`aish <file>` runs a script non-interactively, then exits with the status of
its last line — the shell-script entry point:

```sh
aish deploy.aish        # run the file's lines, then exit
```

Each line is handled exactly as if typed at the prompt: a real command (or
pipeline, or `cd`) runs directly; anything else routes to the model. Blank
lines and `#` comments are skipped, and the `!`/`?` route prefixes work. A
script is treated as explicit, so a bare command word like `who` runs the
`who` program rather than being second-guessed as English.

Because the leading `#!` line is a `#` comment, a script can carry a shebang
and be run as a program directly:

```aish
#!/usr/bin/env aish
# back up the project, then summarize what changed
tar czf /backups/proj.tgz .
summarize what just got archived and flag anything unexpected
```

```sh
chmod +x backup.aish
./backup.aish           # the kernel execs aish with the script path
```

## Make aish your login shell

`aish` exports the standard shell identity vars so tools that inspect the
environment behave: `SHELL` points at the running binary, and `$$` / `$PPID`
expand to the shell's and parent's process ids in direct dispatch (`echo $$`).

`make install` registers the installed binary in `/etc/shells` (idempotent,
best-effort — it may prompt for `sudo`) so you can adopt it as a login shell:

```sh
make install                 # builds, installs, signs, and registers in /etc/shells
chsh -s "$(command -v aish)"  # make it your login shell
```

To register an already-installed binary without rebuilding: `make register-shell`.

## Status

Working prototype. Known gaps vs a classic shell (see the gap analysis):
no pipes/redirection/`$VAR`/globs in direct dispatch (those lines route to the
model), no job control (Ctrl-Z), path-only tab completion. History and memories
are stored unencrypted in `~/.aish/aish.db`.

## License

Licensed under the [Apache License, Version 2.0](LICENSE-APACHE).
Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this crate shall be licensed as above, without any
additional terms or conditions.
