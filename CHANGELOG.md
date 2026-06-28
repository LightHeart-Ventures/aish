# Changelog

## [Unreleased]

### Added
- **Structured tool results are threaded to the model + Ctrl-O keeps the raw view** (S7.3 / TASK-141): the optional typed payload S7.2 attaches to record/table tools (`list_dir`, `glob_expand`, `grep_files`, `stat_file`, `diff_files`, and MCP JSON passthrough) is now sent to the model as **compact JSON** instead of the alignment/ellipsis-corrupted ASCII rendering — so the LLM parses trustworthy structure rather than re-deriving it from a table. The model-facing representation lives in one place (`ToolResult::model_content`); both the Claude and Grok renderers thread it. The split is deliberate: the **model** gets the JSON, while the **Ctrl-O raw view** (`engine::raw_body`) keeps showing the verbatim, human-readable `content` for every tool — never a JSON dump — with a structured-only fallback to pretty-printed JSON when a result has no rendered text. Plain text-only tools are unchanged for both paths. _Deferred (OQ3):_ there is no per-result-set token-cost cap yet — compact JSON for a large `grep_files`/`glob_expand` payload can be heavier than the rendered text; this is flagged with a `TODO` in `ToolResult::model_content` and will be revisited post-S7.3.
- **Offline skill-install recommendation on no local match** (`src/skill_match.rs::recommend_install`): when no INSTALLED skill fits a substantial task, aish now ranks the binary-shipped registry index (`~/.aish/registry/index.json`, read offline via `skill_provider::local_index_catalog` — no per-turn network) and, when a relevant skill clears the same name-level bar the local nudge uses, folds in a `[aish skill-awareness] … :skill add <ref>` recommendation. This closes the "no local skill → recommend installing one" half of the skill-awareness design (the local-match half already existed). Deduped per session (`Session::skill_suggested`) so the same skill is suggested at most once; gated by a skill-worthy token-count heuristic so trivial commands never trigger it. A full live mcpmarket/skill.fish search stays explicit via `:skill search`.
- **`search-skills` reference skill** (`examples/skills/search-skills/`): the user-invoked, richer-output sibling of the automatic awareness above — ranks INSTALLED skills in a table with star ratings and, on no local match, recommends an INSTALLABLE one. Reconciled to sit on top of the engine rather than compete with it: it reads the **same** offline registry index, re-states the engine's single name-weighted relevance rule (no second scoring formula), triggers on prose (it does NOT shadow the live-network `:skill search` verb), and is written for aish's Rust reality (no `invoke_skill` host call — "using" a skill is reading its `SKILL.md` and following it).

### Changed
- **Skill usage is stated plainly in the prompt + nudge**: the system-prompt Skills section and the per-turn `[aish skill-awareness]` note now spell out that USING a skill simply means reading its `SKILL.md` and following its steps — there is no separate command to "invoke" a skill — so the agent stops claiming it "can't run a skill from this interface" and reaches for the installed playbook (or recommends installing one) instead of silently hand-rolling the work.

### Added
- **miette-backed diagnostics** (S7.1 / TASK-139): aish now has a first-class diagnostic surface (`src/diag.rs`, `AishDiagnostic`) built on [miette](https://crates.io/crates/miette) + [thiserror](https://crates.io/crates/thiserror). A forced-shell parse failure (`!cmd`), a malformed `~/.aishrc` line, or a forced command-not-found now renders with a byte-span **caret**, a stable **`aish::…` code**, and a did-you-mean **`help:`** line instead of a bare drop or an ad-hoc `eprintln!`. Six stable codes: `aish::parse::{unbalanced_quote,unsupported_meta,empty_stage,bad_var_ref}`, `aish::config::bad_export`, `aish::exec::not_found`. Rendering honors the existing color policy (`NO_COLOR` / `--no-color` / non-TTY → plain text, still caret+code+help; color on → graphical theme).
- **Span-aware tokenizer** (`rc::tokenize_diagnosed`): the one tokenizer is now span-aware; `rc::tokenize`/`tokenize_with`/`tokenize_pipeline` are `.ok()` shims over it, so the silent route-to-model path is byte-for-byte unchanged while the forced (`!`) path can explain *why* a line wasn't a command. Exec misses on a forced command surface a cheap, bounded (edit-distance ≤ 2) `$PATH` did-you-mean.

### Changed
- **`~/.aishrc` parse errors are now coded + located**: the previously side-effecting dim `eprintln!` skips in `rc::parse_into` become `aish::config::bad_export` diagnostics with a `~/.aishrc:N` header and a caret on the offending token; rc parsing still continues past a bad line (a single malformed export never drops the rest of the file). A `parse_into_diagnosed` seam makes the emission testable.

## [0.14.0] - 2025-01-16

### Added
- **Skill-awareness layer** (`src/skill_match.rs`): each turn, aish scores the user's request against the installed local skill catalog (`~/.aish/skills`) and, when a skill clearly fits, folds a short `[aish skill-awareness] …` note into that turn's input pointing the model at the matching `SKILL.md`. Matching is keyword-overlap based — name-token hits weigh more than description hits, with a single name match enough to surface a hint and up to two top matches named. The note goes into the turn *input* (alongside `engine::seed_context`), never the cached system prompt, so the prompt-cache prefix stays byte-stable. Registry auto-search on no-match is deliberately left to the explicit `:skill search` / `--skill-search` path (no per-turn network round-trips).
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
