# Changelog

## [Unreleased]

### Added
- **Relevance-ranked memory recall**: `recall` now generates keyword candidates via a new FTS5 index and re-ranks them by embedding cosine similarity, so the most relevant fact leads instead of merely the newest. The long-dormant `embedding` column / `vec_memories` index are now populated (a dependency-free local lexical embedder, pluggable for a learned model later) on every `remember` and backfilled for existing rows on open. Falls back to a substring scan + recency when FTS5/embeddings are unavailable.
- **`recall` `tag` argument**: pass `tag="context-offload"` (or query `"context-offload"`) to retrieve compacted-conversation transcripts, which are now kept out of normal curated recall.

### Changed
- **History-compaction offloads are quarantined** into a dedicated `offloads` table instead of co-mingling with curated `memories`, so a routine `recall` can never drag an MB-scale transcript in front of real facts. Existing `context-offload` rows are migrated out of `memories` on open (idempotent).
- **Every `recall` hit is truncated** to a ~2 KB cap with an elision marker — a rehydrated transcript can no longer dump a six-figure-token blob into a single tool result (the offload token-blowup fix).
- **Offload transcripts are bounded** by a keep-recent (20) + max-age (7 day) reaper run on each write, so the store can't grow without limit. Curated dedup (`organize_memories`) no longer scans transcript bytes (they live in their own table).

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
