# Session-scoped background jobs — implementation plan

Concrete code changes in priority order. ✅ = landed in the **skeleton PR**;
⏳ = follow-up. The skeleton is deliberately a no-behaviour-change add: it ships
the session-id export and the pure filtering logic, but keeps the
`background_status` default at `All`.

## Phase 0 — skeleton (this PR)

### 0.1 ✅ Session init / env export — `src/main.rs`
- `Session::session_id` already exists (UUIDv4 in `Session::new()`).
- Export it so children can tag work:
  ```rust
  session.set_var("AISH_SESSION_ID", session.session_id.clone());
  ```
- In the `--coordinator` adoption block, after `session.session_id = sid`,
  **re-export** `AISH_SESSION_ID` so the coordinator and the jobs it spawns tag
  back to the launching session, not the child's throwaway uuid.

### 0.2 ✅ Pure scope logic — `src/scope.rs` (new)
- `enum JobScope { Session, All, Repo(String), Job(String) }`.
- `JobScope::parse(raw: Option<&str>) -> JobScope` — the "status of X" grammar.
- `struct JobRef<'a> { owner_session_id: Option<&'a str>, repo_key: Option<&'a str>, id: &'a str }`.
- `JobScope::matches(&self, job: &JobRef, current_session_id: &str) -> bool`.
- Unit tests for parse + matches + legacy-null behaviour.
- Register `mod scope;` in `main.rs`.

### 0.3 ✅ `background_status` signature + schema — `src/tools.rs`
- Tool schema gains an optional `scope` string property (documented enum-ish).
- `fn background_status(session: &Session)` → `fn background_status(call: &ToolCall, session: &Session)`.
- Parse `call.args["scope"]` via `JobScope::parse`.
- Apply `Session` / `All` / `Job` filtering to the worker + coordinator + batch
  loops using `JobScope::matches`. `Repo` returns a "not yet wired" note
  (needs 1.1).
- **Default (no `scope` arg) = `All`** → existing behaviour preserved.

## Phase 1 — durable repo tagging (follow-up)

### 1.1 ⏳ `repo_key` column — `src/db.rs`
- Add nullable `repo_key TEXT` to `coordinator_runs` and `batch_jobs` (idempotent
  `ALTER TABLE … ADD COLUMN`, mirroring the `session_id`/`session_name` back-compat
  adds already there).
- Thread it through `insert(...)`, `CoordinatorRow`, `BatchRow`, `load_all`.
- Populate from `crate::worker::repo_key(&cwd)` at spawn (`tools.rs`/`coordinator.rs`/`batch.rs`).

### 1.2 ⏳ Wire `Repo` filtering
- In `background_status`, set `JobRef.repo_key` from the new column and drop the
  `Repo` stub note.

## Phase 2 — make session the default (follow-up)

### 2.1 ⏳ Flip the default
- `JobScope::parse(None)` already returns `Session`; change `background_status`
  to treat an absent `scope` arg as `Session` instead of `All`.

### 2.2 ⏳ System-prompt nudge — `src/session.rs`
- Teach the model: bare "status" → omit `scope` (session); "all sessions" →
  `scope:"all"`; "status of <name>" → `scope:"<name>"`.

### 2.3 ⏳ REPL parity — `src/repl.rs`
- `:status [scope]` / `:workers [scope]` honour the same `JobScope` grammar so
  the human-facing meta-commands match the tool.

## Testing

- `src/scope.rs` unit tests (parse + matches + legacy null) — in the skeleton.
- Follow-up: `background_status` integration test asserting Session vs All row
  sets against a seeded `aish.db`; `db.rs` round-trip test for `repo_key`.

## Rollout / risk

- Skeleton is non-breaking (default unchanged); safe to merge behind review.
- Phase 1 schema change is additive + idempotent — old binaries ignore the new
  column, new binaries tolerate its absence (back-compat `ALTER`).
- Phase 2 is the only behaviour change; ship it once the prompt nudge teaches
  the model the new default, so "what's running everywhere?" still works.
