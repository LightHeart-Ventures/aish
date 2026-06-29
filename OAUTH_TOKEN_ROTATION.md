# OAuth Token Rotation Handling

## Overview

Aish supports both static API keys and OAuth tokens (subscription credentials) for both Claude and Grok backends. Token rotation handling differs between the two providers:

## Claude OAuth (Subscription Tokens)

Claude Max/Pro subscription tokens (`sk-ant-oat-...`) are **long-lived** and do NOT rotate automatically. When they expire:

1. **Detection**: Aish detects 401/403 auth failures or 400 errors with "invalid"/"expired"/"unauthorized" keywords
2. **User Notification**: A clear message is printed to stderr:
   ```
   ⚠ Claude OAuth token expired
     Your Claude Max/Pro subscription token needs to be refreshed.
     Run: claude setup-token
     Then set CLAUDE_CODE_OAUTH_TOKEN in your shell or ~/.aishrc
   ```
3. **User Action Required**: The user manually runs `claude setup-token` to get a fresh token and updates their environment

### Implementation

See `src/backend/claude.rs`:
- `Credential::resolve()` — checks `CLAUDE_CODE_OAUTH_TOKEN` env var or `~/.aishrc` export
- `post_with_retry()` — detects auth failures and prints guidance
- Tests: `oauth_detects_401_as_auth_failure()`, `oauth_detects_400_with_expired_keyword()`

## Grok OAuth (xAI Subscription Tokens)

Grok subscription tokens stored in `~/.grok/auth.json` support **automatic token rotation**:

1. **Automatic Refresh**: On 401/403/400 auth errors, aish calls the token-refresh endpoint
2. **Refresh Token Rotation**: The server returns a new refresh token (which rotates on each use)
3. **Atomic Persistence**: The new tokens are persisted back to `~/.grok/auth.json` atomically (temp + rename, 0600 perms)
4. **Transparent Retry**: The run continues immediately with the refreshed token

### Implementation

See `src/backend/grok.rs`:
- `GrokOAuthStore::refresh()` — exchanges refresh token for new access token
- `GrokOAuthStore::write_back()` — persists rotated tokens atomically
- `post_with_retry()` — detects auth failures and triggers refresh for OAuth-file credentials
- Tests: `token_extracted_from_grok_auth_json()`, `apply_refresh_updates_entry_and_preserves_other_fields()`

## Configuration

### Claude
```bash
# Option 1: Environment variable
export CLAUDE_CODE_OAUTH_TOKEN="sk-ant-oat-..."

# Option 2: ~/.aishrc export
echo 'export CLAUDE_CODE_OAUTH_TOKEN="sk-ant-oat-..."' >> ~/.aishrc

# Option 3: Fallback to API key
export ANTHROPIC_API_KEY="sk-ant-..."
```

### Grok
```bash
# The Grok CLI manages ~/.grok/auth.json
grok login

# Aish reads it fresh on each request (picks up CLI-refreshed tokens)
# and automatically refreshes when needed
```

## Future Enhancements

- **Claude OAuth with refresh tokens**: If Anthropic introduces refresh-token support similar to Grok, aish could implement automatic rotation
- **~/.claude/auth.json support**: Mirror Grok's file-based OAuth store for better token lifecycle management
