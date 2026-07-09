# Prerequisites — `codebase-memory-mcp` binary

The plugin only supplies the aish wiring. The actual server is the
**DeusData/codebase-memory-mcp** binary (MIT). Install it once by any of the
methods below, then confirm it resolves on your `PATH`.

```
codebase-memory-mcp --version
```

The plugin's `.mcp.json` invokes the bare command `codebase-memory-mcp`, so it
must be reachable on `PATH`. If you install to a non-PATH location, either add
that dir to `PATH` or edit the `command` in `~/.aish/plugins/codebase-memory/.mcp.json`
to an absolute path.

## Option A — Homebrew (macOS / Linuxbrew)

```
brew install deusdata/tap/codebase-memory-mcp
# or, if published to the default tap:
brew install codebase-memory-mcp
```

Homebrew installs to `$(brew --prefix)/bin` (e.g. `/opt/homebrew/bin` on Apple
Silicon, `/usr/local/bin` on Intel), which is already on `PATH`. Verify:

```
which codebase-memory-mcp
```

> If the tap/formula name differs upstream, check the project's README:
> https://github.com/DeusData/codebase-memory-mcp

## Option B — aish-native installer (recommended if no Homebrew)

aish ships a native enroller that downloads the **platform-matched release
asset** and installs it into `~/.aish/bin/`:

```
:codebase install
:codebase status
```

- Assets are pulled from `https://github.com/DeusData/codebase-memory-mcp/releases`
  (pinned tag `v0.1.0` in this aish build; see `src/codebase_memory.rs`).
- Supported targets: linux `x86_64`/`aarch64`, macOS `x86_64`/`aarch64`,
  windows `x86_64`.
- The binary lands at `~/.aish/bin/codebase-memory-mcp`. Add `~/.aish/bin` to
  `PATH`, **or** point the plugin `.mcp.json` `command` at the absolute path.

`:codebase install` also writes the `mcpServers.codebase-memory` entry into
`~/.aish/.mcp.json` — which is exactly what the repo-open **auto-index** gate
needs for enrollment (see README → Auto-index gate).

## Option C — Build from source

```
git clone https://github.com/DeusData/codebase-memory-mcp
cd codebase-memory-mcp
cargo build --release
# then put the binary on PATH, e.g.:
install -m 0755 target/release/codebase-memory-mcp ~/.local/bin/
```

## Manual download

Grab the asset for your platform directly:

```
https://github.com/DeusData/codebase-memory-mcp/releases/download/<version>/codebase-memory-mcp-<target>.tar.gz
```

Targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` (`.zip`).
Extract, `chmod +x`, and place on `PATH`.

## Verify end-to-end

After install and restarting aish (so the plugin loads):

```
:mcp                 # codebase-memory should appear with a tool count
```

If it's missing: confirm the binary runs (`codebase-memory-mcp --version`),
that it's on `PATH` (or the `.mcp.json` command is an absolute path), and check
`:mcp` output / startup warnings for a spawn error.
