# aish Installation Guide for Ubuntu 24.04 LTS

This guide covers installing and configuring **aish** (AI-native shell) on Ubuntu 24.04 LTS.

## Prerequisites

- Ubuntu 24.04 LTS (or compatible Debian-based distro)
- `curl` or `wget` (usually pre-installed)
- An Anthropic Claude API key for authentication
- ~50 MB disk space
- Bash or Zsh for the parent shell

## Installation Methods

### Method 1: Using the Official Installer (Recommended)

The official aish project provides a one-liner installer:

```bash
curl -fsSL https://install.aish.sh | bash
```

This script:
- Downloads the latest aish binary for your platform
- Places it in a standard PATH location (typically `~/.local/bin/aish`)
- Creates necessary directories (`~/.aish/`)
- Sets up initial configuration

**After installation, restart your shell or run:**
```bash
source ~/.bashrc
# or if using zsh:
source ~/.zshrc
```

### Method 2: Manual Download

If you prefer explicit control:

```bash
# 1. Create aish directory
mkdir -p ~/.aish

# 2. Download the latest binary (check GitHub releases for your arch)
# For x86_64 Linux:
curl -fsSL https://github.com/anthropics/aish/releases/download/v0.14.0/aish-x86_64-unknown-linux-gnu \
  -o ~/.local/bin/aish

# 3. Make it executable
chmod +x ~/.local/bin/aish

# 4. Verify installation
aish --version
```

### Method 3: Using Package Manager (If Available)

```bash
# Homebrew (if brew is installed on Linux)
brew install aish

# Or check if your distro has aish in repositories
apt search aish
```

---

## Initial Configuration

### Step 1: Set Up Your Anthropic API Key

aish requires an Anthropic Claude API key. You can provide it in three ways:

#### Option A: Interactive Prompt (Easiest)
When you run `aish` for the first time, it will ask for your API key interactively.

#### Option B: Environment Variable (Recommended for Scripting)
Add to your **~/.bashrc** or **~/.zshrc**:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
```

Then reload:
```bash
source ~/.bashrc
```

#### Option C: Credentials File
Create `~/.aish/credentials` with your configuration:

```bash
mkdir -p ~/.aish
cat > ~/.aish/credentials << 'EOF'
[default]
api_key = sk-ant-...
EOF

chmod 600 ~/.aish/credentials
```

### Step 2: Create ~/.aishrc (Optional)

While aish doesn't auto-source `~/.aishrc` like bash does, you can create one for reference or to be sourced manually:

```bash
cat > ~/.aishrc << 'EOF'
# aish configuration file (manually sourced)
# To load: source ~/.aishrc

# Export variables for aish sessions
export ANTHROPIC_API_KEY="sk-ant-..."
export AISH_MODEL="claude-opus-4-6"  # Optional: choose specific model

# Custom aliases (if aish supports them)
alias ll='ls -lah'
alias gs='git status'

# Add your custom configuration here
EOF

chmod 600 ~/.aishrc
```



### Step 3: Verify Installation

```bash
# Check version
aish --version

# Start an aish session
aish

# Inside aish, you should see the prompt:
# ⚡ 

# Type :help to see available commands
:help

# Type :exit or Ctrl+D to exit
```

---

## Post-Installation Configuration

### Environment Variables for Parent Shell

Add to **~/.bashrc** or **~/.zshrc** to set up your environment BEFORE launching aish:

```bash
# Anthropic API Configuration
export ANTHROPIC_API_KEY="sk-ant-..."

# Optional: Set default model
export AISH_MODEL="claude-opus-4-6"

# Optional: Enable debug logging
# export AISH_DEBUG=1

# Optional: Set default MCP servers
# export AISH_MCP_SERVERS="path/to/mcp/config.json"
```

Then reload your shell:
```bash
bash -l
# or
zsh -l
```

### MCP Servers Setup (Optional)

If you want to use MCP (Model Context Protocol) servers with aish, configure them in:

```bash
mkdir -p ~/.aish
cat > ~/.aish/.mcp.json << 'EOF'
{
  "mcpServers": {
    "aws-mcp": {
      "command": "uvx",
      "args": ["aws-mcp"]
    },
    "github": {
      "command": "uvx",
      "args": ["mcp-server-github", "--token", "ghp_..."]
    }
  }
}
EOF
```

---

## Troubleshooting

### Issue: "aish: command not found"

**Solution:**
1. Verify installation:
   ```bash
   ls -la ~/.local/bin/aish
   ```
2. Ensure `~/.local/bin` is in your PATH:
   ```bash
   echo $PATH | grep -o "\.local/bin"
   ```
3. If not, add to **~/.bashrc** or **~/.zshrc**:
   ```bash
   export PATH="$HOME/.local/bin:$PATH"
   ```
4. Reload shell:
   ```bash
   source ~/.bashrc
   ```

### Issue: "No ANTHROPIC_API_KEY set"

**Solution:**
- Export your API key in the parent shell BEFORE launching aish:
  ```bash
  export ANTHROPIC_API_KEY="sk-ant-..."
  aish
  ```
- Or configure it in `~/.aish/credentials` (see Step 1, Option C)



### Issue: "Permission denied" when running aish

**Solution:**
```bash
chmod +x ~/.local/bin/aish
```

### Issue: aish crashes or hangs

**Solution:**
1. Check if your API key is valid:
   ```bash
   curl https://api.anthropic.com/v1/models \
     -H "Authorization: Bearer $ANTHROPIC_API_KEY" | head -20
   ```
2. Check network connectivity:
   ```bash
   ping -c 1 api.anthropic.com
   ```
3. Try with debug logging:
   ```bash
   AISH_DEBUG=1 aish
   ```

---

## Usage Basics

Once aish is running, you're in an AI-native shell:

```bash
# Regular shell commands work
ls -la
pwd
cat myfile.txt

# Use aish features with ⚡ prefix
⚡ help me analyze this log file
⚡ create a Python script to...
⚡ explain this error message

# Built-in commands start with :
:help          # Show all commands
:clear         # Clear screen
:exit          # Exit aish
:jobs          # List background jobs
```

---

## Updating aish

To update to the latest version:

```bash
# Method 1: Re-run installer
curl -fsSL https://install.aish.sh | bash

# Method 2: Manual update
curl -fsSL https://github.com/anthropics/aish/releases/download/latest/aish-x86_64-unknown-linux-gnu \
  -o ~/.local/bin/aish
chmod +x ~/.local/bin/aish

# Verify
aish --version
```

---

## Security Best Practices

1. **Protect your API key:**
   - Never commit it to version control
   - Use `chmod 600` on credential files
   - Consider using environment variables only when needed

2. **Use restricted API keys:**
   - In Anthropic console, create API keys with minimal required permissions
   - Rotate keys regularly

3. **Monitor usage:**
   - Check your Anthropic account for unexpected activity
   - Set usage limits if available

4. **Keep aish updated:**
   - Regularly check for security updates
   - Update as soon as critical patches are available

---

## Advanced Configuration

### Custom MCP Servers

See the MCP setup section above for configuring additional MCP servers.

### Model Selection

Specify which Claude model to use:

```bash
# In parent shell
export AISH_MODEL="claude-opus-4-6"
aish
```

### Logging and Debugging

Enable debug output:

```bash
AISH_DEBUG=1 aish
```

---

## Uninstalling aish

To remove aish from your system:

```bash
# Remove binary
rm ~/.local/bin/aish

# Remove config and cache (optional; keeps your data)
rm -rf ~/.aish

# Remove from PATH (if you added it manually to ~/.bashrc)
# Edit ~/.bashrc and remove the PATH line
```

---

## Resources

- **Official aish GitHub:** https://github.com/anthropics/aish
- **Anthropic API Docs:** https://docs.anthropic.com
- **Claude Model Guide:** https://docs.anthropic.com/claude/reference
- **aish Issues & Support:** https://github.com/anthropics/aish/issues

---

## Summary

| Step | Command |
|------|---------|
| Install | `curl -fsSL https://install.aish.sh \| bash` |
| Set API key | `export ANTHROPIC_API_KEY="sk-ant-..."` |
| Launch | `aish` |
| Get help | `:help` (inside aish) |
| Exit | `:exit` or `Ctrl+D` |

---

**Last Updated:** Ubuntu 24.04 LTS | aish v0.14.0
