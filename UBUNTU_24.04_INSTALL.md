# Ubuntu 24.04 LTS Installation Guide

This guide covers installing aish on **Ubuntu 24.04 LTS** (Noble Numbat).

## Quick Start

```bash
# 1. Install build dependencies
sudo apt-get update
sudo apt-get install -y build-essential cmake rustup git pkg-config libssl-dev perl

# 2. Install Rust (if not present)
rustup-init -y
source $HOME/.cargo/env

# 3. Clone and build aish
git clone https://github.com/LightHeart-Ventures/aish.git
cd aish
make install

# 4. Set your Claude API key
export ANTHROPIC_API_KEY=sk-ant-...

# 5. Launch
aish
```

## Prerequisites

### Ubuntu 24.04 Package Installation

```bash
# Essential build tools + Rust
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  cmake \
  rustup \
  git \
  ca-certificates \
  curl \
  pkg-config \
  libssl-dev \
  perl

# Verify installations
rustc --version
cargo --version
gcc --version
cmake --version
```

> **Note:** `cmake` and `perl` are required even for the Claude-only build —
> the HTTP stack vendors and compiles BoringSSL, whose build uses CMake.
> Omitting `cmake` causes the build to fail with `is cmake not installed?`.

> **Note:** Ubuntu 24.04 ships `rustup` as a native apt package (the same
> upstream rustup wrapper). If you prefer the official installer, use
> `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh` instead.

**Optional** (for local model inference):
```bash
# If you plan to run Qwen3-1.7B offline:
sudo apt-get install -y \
  libsqlite3-dev
```

### Rust Toolchain Setup

```bash
# Initialize rustup (one-time)
rustup-init -y
source $HOME/.cargo/env

# Verify Rust is installed
cargo --version   # should show 1.80+
rustc --version
```

## Building from Source

### Standard Build (Claude API only)

Fast, minimal footprint:

```bash
cd aish
cargo build --release --no-default-features
./target/release/aish --version
```

### Full Build (with local model support)

Enables offline Qwen3-1.7B inference:

```bash
cd aish
cargo build --release
./target/release/aish --version
```

**Build time**: ~3–5 minutes on a typical laptop (first build downloads Rust dependencies).

### Installing the Binary

```bash
# Install to ~/.local/bin and register in /etc/shells
make install

# Verify it's on PATH
which aish
aish --version

# (Optional) Make it your login shell
chsh -s "$(which aish)"
```

## Configuration

### Environment Variables

```bash
# Required: Claude API key
export ANTHROPIC_API_KEY=sk-ant-...

# Optional: Grok API key (if using `--backend grok`)
export XAI_API_KEY=...

# Optional: Local model ID (if using `--backend local`)
export AISH_LOCAL_MODEL_ID=Qwen/Qwen3-1.7B-GGUF

# Make these permanent in ~/.bashrc or ~/.zshrc
echo 'export ANTHROPIC_API_KEY=sk-ant-...' >> ~/.bashrc
source ~/.bashrc
```

### First Launch

```bash
aish   # interactive shell
```

On first run:
- aish seeded from your existing `.bashrc` (aliases + exports are imported)
- SQLite database created at `~/.aish/aish.db`
- Skills registry initialized at `~/.aish/registry/index.json`

## Troubleshooting

### Build Errors

#### "error: linker `cc` not found"
```bash
sudo apt-get install -y build-essential
```

#### "error: failed to run custom build command for `...` (is cmake not installed?)"
```bash
sudo apt-get install -y cmake perl
```

#### "error: failed to verify the checksum of rustup"
```bash
rustup self update
rustup update stable
```

### Runtime Issues

#### "command not found: aish"
Verify it's on PATH:
```bash
which aish
echo $PATH

# If missing, add to ~/.bashrc:
export PATH="$HOME/.local/bin:$PATH"
source ~/.bashrc
```

#### "ANTHROPIC_API_KEY not set"
```bash
export ANTHROPIC_API_KEY=sk-ant-...
aish
```

#### Container worker image build fails (Docker/Podman)

aish can run without containers; the worker image is optional:

```bash
# Run with host-based workers (AC1 fallback)
export AISH_CONTAINER_RUNTIME=none
aish
```

To fix the image build:
```bash
# Ensure Docker or Podman is installed
sudo apt-get install docker.io
sudo usermod -aG docker $USER
# Log out and back in to apply group membership

# Try building the worker image
make worker-image
```

## Verification

### Check Installation

```bash
aish --version                       # prints version
aish -c "echo hello from aish"      # one-shot command
aish --help                          # usage info
```

### Quick Test

```bash
aish -c "who is alan turing"  # model route (English)
aish -c "ls -la"              # direct dispatch (command)
```

## Uninstall

```bash
make uninstall          # remove binary
rm -rf ~/.aish          # remove config + history
rm -f ~/.aishrc         # remove rc file
```

## Upgrading

### From Source

```bash
cd aish
git pull origin main
make install
```

### From Release Binary

```bash
# Download latest release
curl -L https://github.com/LightHeart-Ventures/aish/releases/latest/download/aish-x86_64-unknown-linux-gnu \
  -o ~/.local/bin/aish.new
chmod +x ~/.local/bin/aish.new
mv ~/.local/bin/aish.new ~/.local/bin/aish
```

## Advanced Usage

### Offline Mode (No API)

```bash
aish --backend local       # use Qwen3-1.7B (first run downloads ~4 GB GGUF)
```

### Custom Model Selection

```bash
AISH_LOCAL_MODEL_ID=Qwen/Qwen3-4B-GGUF aish --backend local
```

### Strict Confirmation Mode

```bash
aish --mode paranoid       # confirm every tool call
aish --mode careful        # confirm writes only
```

### Non-Interactive (Script) Mode

```bash
aish script.aish           # run a script, then exit
```

## System Requirements

| Requirement | Ubuntu 24.04 | Notes |
|---|---|---|
| **OS** | ✅ | Tested on 24.04 LTS; 20.04 also works |
| **CPU** | Any | x86_64 or ARM64 (e.g., AWS Graviton) |
| **RAM** | 4 GB+ | 8 GB+ recommended for `--backend local` |
| **Disk** | 500 MB+ | 5 GB+ if running local model (model cache) |
| **glibc** | 2.39 | Ubuntu 24.04 ships 2.39 (rustls TLS library compatible) |
| **TLS** | OpenSSL 3.0 | Ubuntu 24.04 ships 3.0.x (compatible; aish uses rustls) |

## Integration with System Tools

### Tab Completion (Bash)

Add to `~/.bashrc`:
```bash
eval "$(aish --completion bash)"  # future; not yet implemented
```

### Login Shell Setup

```bash
chsh -s "$(which aish)"
# On next login, aish becomes your shell
```

### Systemd User Service (for background coordinators)

```ini
# ~/.config/systemd/user/aish-coordinator.service
[Unit]
Description=aish background coordinator
After=network-online.target

[Service]
Type=simple
ExecStart=%h/.local/bin/aish --coordinator
Restart=on-failure
RestartSec=10s
Environment="ANTHROPIC_API_KEY=sk-ant-..."

[Install]
WantedBy=default.target
```

Enable:
```bash
systemctl --user enable aish-coordinator
systemctl --user start aish-coordinator
```

## Performance Notes

- **First run**: ~3 seconds (SQLite init + MCP server startup)
- **Model load** (`--backend local`): ~10 seconds (Qwen3-1.7B loads from disk cache on every launch)
- **API latency** (`--backend claude`): 1–5 seconds (depends on Claude API load)
- **Direct dispatch**: <100ms (shell commands like `ls`, `cd`)

## Next Steps

- [Read the README](README.md) for full feature overview
- Check `:help` in the aish REPL
- Try the scripting example in the README
- Explore MCP server integration via `~/.aish/.mcp.json`

## Support

- **Issues**: [GitHub Issues](https://github.com/LightHeart-Ventures/aish/issues)
- **Discussions**: [GitHub Discussions](https://github.com/LightHeart-Ventures/aish/discussions)
- **License**: Apache 2.0 / MIT (dual)
