# Changelog

## [0.13.1] - 2025-01-16

### Added
- **Installed Status Display in `:skill search`**: Search results now show a green `✓ installed` indicator for skills already in your local `~/.aish/skills` directory, making it easy to see what's installed vs available
- **Exponential Backoff Retry for mcpmarket Bot Protection**: When Vercel's bot challenge (HTTP 429) blocks mcpmarket.com access, aish now retries with exponential backoff (1s, 2s, 4s delays), matching skillfish's behavior
- Debug logging for skill search retry attempts and responses

### Fixed
- Graceful fallback to embedded offline skill catalog when mcpmarket is unavailable or returns transient errors

### Changed
- Enhanced `:skill search` table layout with clearer column organization

## [0.13.0] - Previous release
