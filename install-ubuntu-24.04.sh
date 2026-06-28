#!/bin/bash
# aish — one-command Ubuntu 24.04 installer
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/LightHeart-Ventures/aish/main/install-ubuntu-24.04.sh | bash
#   # or
#   bash install-ubuntu-24.04.sh
#
# This script:
# 1. Installs build dependencies (build-essential, rustup, git)
# 2. Clones aish from GitHub
# 3. Builds and installs the binary to ~/.local/bin
# 4. Registers aish in /etc/shells for use as a login shell
# 5. Prompts for ANTHROPIC_API_KEY setup

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}╔════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║         aish — AI-native Linux Shell                       ║${NC}"
echo -e "${BLUE}║         Ubuntu 24.04 LTS Installer                         ║${NC}"
echo -e "${BLUE}╚════════════════════════════════════════════════════════════╝${NC}"
echo ""

# Step 1: Check Ubuntu version
echo -e "${YELLOW}[1/5]${NC} Verifying Ubuntu 24.04 LTS..."
if ! grep -q "noble" /etc/os-release 2>/dev/null; then
    if ! grep -q "24.04" /etc/os-release 2>/dev/null; then
        echo -e "${YELLOW}⚠️  This system may not be Ubuntu 24.04 LTS${NC}"
        echo "    (It may still work with Ubuntu 22.04+ or Debian 12+)"
        read -p "Continue anyway? (y/n) " -n 1 -r; echo
        if [[ ! $REPLY =~ ^[Yy]$ ]]; then exit 1; fi
    fi
fi
echo -e "${GREEN}✓${NC} Ubuntu version OK"
echo ""

# Step 2: Install dependencies
echo -e "${YELLOW}[2/5]${NC} Installing build dependencies..."
sudo apt-get update -qq
sudo apt-get install -y --no-install-recommends \
    build-essential \
    cmake \
    git \
    ca-certificates \
    curl \
    pkg-config \
    libssl-dev \
    perl \
    >/dev/null
echo -e "${GREEN}✓${NC} Dependencies installed"
echo ""

# Step 3: Setup Rust toolchain
echo -e "${YELLOW}[3/5]${NC} Configuring Rust toolchain..."
if ! command -v cargo &> /dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --quiet
fi
# Bring cargo/rustc onto PATH for the rest of this script
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
rustup update stable --quiet 2>/dev/null || true
echo -e "${GREEN}✓${NC} Rust $(rustc --version | awk '{print $2}')"
echo ""

# Step 4: Clone and build aish
echo -e "${YELLOW}[4/5]${NC} Building aish (this may take 2–5 minutes)..."
TMPDIR=$(mktemp -d)
trap "rm -rf $TMPDIR" EXIT
git clone --depth 1 https://github.com/LightHeart-Ventures/aish.git "$TMPDIR" >/dev/null 2>&1 || {
    echo -e "${RED}✗${NC} Failed to clone repository. Check your internet connection."
    exit 1
}
cd "$TMPDIR"
# Build, streaming a condensed progress line but preserving the real exit status.
# (Piping through grep would mask build failures behind a misleading
#  "install: cannot stat 'target/release/aish'" error further down.)
build_log="$TMPDIR/build.log"
if ! cargo build --release --no-default-features 2>&1 | tee "$build_log" | grep -E "Compiling aish|Finished"; then
    : # grep may exit non-zero if those lines never appear; real status checked below
fi
if [ ! -x target/release/aish ]; then
    echo -e "${RED}✗${NC} Build failed — the aish binary was not produced."
    echo "    Last lines of the build log:"
    tail -n 20 "$build_log" | sed 's/^/      /'
    exit 1
fi
mkdir -p "$HOME/.local/bin"
install -m 0755 target/release/aish "$HOME/.local/bin/aish"
echo -e "${GREEN}✓${NC} aish $("$HOME/.local/bin/aish" --version | head -1) installed to ~/.local/bin/aish"
echo ""

# Step 5: Register as login shell
echo -e "${YELLOW}[5/5]${NC} Registering as login shell..."
if ! grep -qxF "$HOME/.local/bin/aish" /etc/shells 2>/dev/null; then
    echo "$HOME/.local/bin/aish" | sudo tee -a /etc/shells >/dev/null
fi
echo -e "${GREEN}✓${NC} Registered in /etc/shells"
echo ""

# Post-install checklist
echo -e "${BLUE}╔════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║                    Installation Complete                   ║${NC}"
echo -e "${BLUE}╚════════════════════════════════════════════════════════════╝${NC}"
echo ""

# Ensure ~/.local/bin is on PATH
if ! grep -q "$HOME/.local/bin" <<< "$PATH"; then
    echo -e "${YELLOW}⚠️  ~/.local/bin is not on PATH${NC}"
    echo "Add this line to ~/.bashrc or ~/.zshrc:"
    echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
    source_cmd="source ~/.bashrc"
    if [ -f ~/.zshrc ]; then source_cmd="source ~/.zshrc"; fi
    echo "Then run: $source_cmd"
    echo ""
fi

echo -e "${GREEN}Next Steps:${NC}"
echo "  1. Set your Claude API key:"
echo "     export ANTHROPIC_API_KEY=sk-ant-..."
echo ""
echo "  2. Launch aish:"
echo "     aish"
echo ""
echo "  3. (Optional) Make it your login shell:"
echo "     chsh -s ~/.local/bin/aish"
echo ""
echo "For more info: https://github.com/LightHeart-Ventures/aish/blob/main/UBUNTU_24.04_INSTALL.md"
echo ""
