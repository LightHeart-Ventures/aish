# Changelog

## [Unreleased]

### Added
- **Skill-awareness layer** (`src/skill_match.rs`): each turn, aish scores the user's request against the installed local skill catalog (`~/.aish/skills`) and, when a skill clearly fits, folds a short `[aish skill-awareness] …` note into that turn's input pointing the model at the matching `SKILL.md`. Matching is keyword-overlap based — name-token hits weigh more than description hits, with a single name match enough to surface a hint and up to two top matches named. The note goes into the turn *input* (alongside `engine::seed_context`), never the cached system prompt, so the prompt-cache prefix stays byte-stable. Registry auto-search on no-match is deliberately left to the explicit `:skill search` / `--skill-search` path (no per-turn network round-trips).

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
